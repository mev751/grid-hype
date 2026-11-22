use crate::{
    config::{Direction, GridConfig},
    precision::{round_hl_price, round_hl_size},
};
use anyhow::{Result, bail};
use nautilus_model::{enums::OrderSide, identifiers::ClientOrderId};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotState {
    /// No active order or position for this grid interval.
    Idle,
    /// Opening Maker order is resting on the book (`Buy` for Long, `Sell` for Short).
    EntryWorking { client_order_id: ClientOrderId },
    /// Entry order filled, waiting for market price to allow placing a valid Post-Only exit order.
    HoldingWaitingExit,
    /// Closing Maker (`reduce_only`) order is resting on the book (`Sell` for Long, `Buy` for Short).
    ExitWorking { client_order_id: ClientOrderId },
}

#[derive(Debug, Clone)]
pub struct GridSlot {
    pub index: usize,
    pub lower_price: f64,
    pub upper_price: f64,
    pub entry_price: f64,
    pub exit_price: f64,
    pub quantity: f64,
    pub state: SlotState,
    pub completed_cycles: u64,
    pub realized_gross_pnl_usdc: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GridOrderIntent {
    pub slot_index: usize,
    pub client_order_id: ClientOrderId,
    pub side: OrderSide,
    pub price: f64,
    pub quantity: f64,
    pub post_only: bool,
    pub reduce_only: bool,
    pub is_entry: bool,
}

#[derive(Debug, Clone)]
pub struct GridSummary {
    pub total_grids: usize,
    pub active_entry_orders: usize,
    pub active_exit_orders: usize,
    pub holding_slots: usize,
    pub completed_cycles: u64,
    pub realized_gross_pnl_usdc: f64,
    pub unrealized_pnl_usdc: f64,
    pub total_working_entry_notional_usdc: f64,
    pub total_position_base_qty: f64,
    pub total_position_cost_usdc: f64,
}

#[derive(Debug, Clone)]
pub struct GridEngine {
    cfg: GridConfig,
    sz_decimals: u8,
    slots: Vec<GridSlot>,
    order_to_slot: HashMap<ClientOrderId, (usize, bool)>,
    order_seq: u64,
    initialized: bool,
}

impl GridEngine {
    pub fn new(cfg: GridConfig, sz_decimals: u8) -> Result<Self> {
        if cfg.grid_count == 0 {
            bail!("grid_count must be >= 1");
        }
        if cfg.lower_price >= cfg.upper_price {
            bail!("lower_price must be < upper_price");
        }
        let step = (cfg.upper_price - cfg.lower_price) / (cfg.grid_count as f64);
        let mut slots = Vec::with_capacity(cfg.grid_count);

        for i in 0..cfg.grid_count {
            let raw_low = cfg.lower_price + (i as f64) * step;
            let raw_high = cfg.lower_price + ((i + 1) as f64) * step;
            let low = round_hl_price(raw_low, sz_decimals);
            let high = round_hl_price(raw_high, sz_decimals);

            if low <= 0.0 || high <= low {
                bail!(
                    "Grid step too narrow at slot {i}: lower={low} upper={high} after rounding (sz_decimals={sz_decimals})"
                );
            }

            let (entry_price, exit_price) = match cfg.direction {
                Direction::Long => (low, high),
                Direction::Short => (high, low),
            };

            let quantity = round_hl_size(cfg.amount_per_grid / entry_price, sz_decimals);
            if quantity <= 0.0 {
                bail!(
                    "Grid slot {i} computed zero quantity for amount={} at entry_price={}",
                    cfg.amount_per_grid,
                    entry_price
                );
            }

            slots.push(GridSlot {
                index: i,
                lower_price: low,
                upper_price: high,
                entry_price,
                exit_price,
                quantity,
                state: SlotState::Idle,
                completed_cycles: 0,
                realized_gross_pnl_usdc: 0.0,
            });
        }

        Ok(Self {
            cfg,
            sz_decimals,
            slots,
            order_to_slot: HashMap::new(),
            order_seq: 0,
            initialized: false,
        })
    }

    pub fn slots(&self) -> &[GridSlot] {
        &self.slots
    }

    pub fn sz_decimals(&self) -> u8 {
        self.sz_decimals
    }

    pub fn config(&self) -> &GridConfig {
        &self.cfg
    }

    fn next_client_order_id(&mut self, slot_index: usize, is_entry: bool) -> ClientOrderId {
        self.order_seq += 1;
        let tag = if is_entry { "E" } else { "X" };
        let dir = match self.cfg.direction {
            Direction::Long => "L",
            Direction::Short => "S",
        };
        ClientOrderId::new(format!(
            "GRID-{}-{}{}-{:03}-{:06}",
            self.cfg.symbol, dir, tag, slot_index, self.order_seq
        ))
    }

    /// Checks whether a limit order at `price` with `side` is safe to submit as a Maker (`Post-Only`)
    /// given the current `best_bid` and `best_ask`.
    fn can_place_maker(side: OrderSide, price: f64, best_bid: f64, best_ask: f64) -> bool {
        match side {
            // A Maker Buy must be strictly below the best ask
            OrderSide::Buy => price < best_ask,
            // A Maker Sell must be strictly above the best bid
            OrderSide::Sell => price > best_bid,
            _ => false,
        }
    }

    /// Evaluates current market top-of-book (`best_bid`, `best_ask`) and emits any Maker order
    /// intents that should be submitted now.
    ///
    /// - On initial call:
    ///   - For `Direction::Long`: places Maker Buy orders on all slots whose `entry_price < mid_price`
    ///     (and `entry_price < best_ask`). Slots above current price remain `Idle` (no market buy at startup).
    ///   - For `Direction::Short`: places Maker Sell orders on all slots whose `entry_price > mid_price`
    ///     (and `entry_price > best_bid`).
    /// - On subsequent ticks:
    ///   - Retries any `HoldingWaitingExit` slot whose exit order can now be placed as Maker without crossing.
    ///   - If `follow_price_into_idle_grids` is true (or for any slot below current market price that became `Idle`
    ///     after a fill/rejection), places the Maker entry order once price is on the valid Maker side.
    pub fn on_market_price(&mut self, best_bid: f64, best_ask: f64) -> Vec<GridOrderIntent> {
        if !best_bid.is_finite() || !best_ask.is_finite() || best_bid <= 0.0 || best_ask <= 0.0 {
            return Vec::new();
        }
        let mid_price = (best_bid + best_ask) * 0.5;
        let is_first = !self.initialized;
        self.initialized = true;

        let mut intents = Vec::new();
        let direction = self.cfg.direction;
        let post_only = self.cfg.post_only;
        let follow_idle = self.cfg.follow_price_into_idle_grids;

        for idx in 0..self.slots.len() {
            let state = self.slots[idx].state.clone();
            let entry_price = self.slots[idx].entry_price;
            let exit_price = self.slots[idx].exit_price;
            let quantity = self.slots[idx].quantity;

            match state {
                SlotState::Idle => {
                    let should_arm_entry = match direction {
                        Direction::Long => {
                            if is_first || follow_idle {
                                entry_price < mid_price
                                    && Self::can_place_maker(
                                        OrderSide::Buy,
                                        entry_price,
                                        best_bid,
                                        best_ask,
                                    )
                            } else {
                                false
                            }
                        }
                        Direction::Short => {
                            if is_first || follow_idle {
                                entry_price > mid_price
                                    && Self::can_place_maker(
                                        OrderSide::Sell,
                                        entry_price,
                                        best_bid,
                                        best_ask,
                                    )
                            } else {
                                false
                            }
                        }
                    };

                    if should_arm_entry {
                        let cloid = self.next_client_order_id(idx, true);
                        let side = match direction {
                            Direction::Long => OrderSide::Buy,
                            Direction::Short => OrderSide::Sell,
                        };
                        self.slots[idx].state = SlotState::EntryWorking {
                            client_order_id: cloid,
                        };
                        self.order_to_slot.insert(cloid, (idx, true));
                        intents.push(GridOrderIntent {
                            slot_index: idx,
                            client_order_id: cloid,
                            side,
                            price: entry_price,
                            quantity,
                            post_only,
                            reduce_only: false,
                            is_entry: true,
                        });
                    }
                }
                SlotState::HoldingWaitingExit => {
                    let exit_side = match direction {
                        Direction::Long => OrderSide::Sell,
                        Direction::Short => OrderSide::Buy,
                    };
                    if Self::can_place_maker(exit_side, exit_price, best_bid, best_ask) {
                        let cloid = self.next_client_order_id(idx, false);
                        self.slots[idx].state = SlotState::ExitWorking {
                            client_order_id: cloid,
                        };
                        self.order_to_slot.insert(cloid, (idx, false));
                        intents.push(GridOrderIntent {
                            slot_index: idx,
                            client_order_id: cloid,
                            side: exit_side,
                            price: exit_price,
                            quantity,
                            post_only,
                            reduce_only: true,
                            is_entry: false,
                        });
                    }
                }
                SlotState::EntryWorking { .. } | SlotState::ExitWorking { .. } => {}
            }
        }

        intents
    }

    /// Called when an order with `client_order_id` is completely filled.
    /// - If it was an **Entry** order for slot `i`: immediately arms the **Exit** (`reduce_only`) Maker order
    ///   at slot `i`'s `exit_price` (level `i + 1` for Long, level `i - 1` for Short), or transitions to
    ///   `HoldingWaitingExit` if the current book would cause a Post-Only cross.
    /// - If it was an **Exit** order for slot `i`: increments `completed_cycles` and `realized_gross_pnl_usdc`,
    ///   resets slot `i` to `Idle`, and immediately re-arms the **Entry** Maker order at `entry_price` if
    ///   current book allows Maker placement.
    pub fn on_order_filled(
        &mut self,
        client_order_id: &ClientOrderId,
        best_bid: f64,
        best_ask: f64,
    ) -> Vec<GridOrderIntent> {
        let Some((slot_idx, is_entry)) = self.order_to_slot.remove(client_order_id) else {
            return Vec::new();
        };

        let direction = self.cfg.direction;
        let post_only = self.cfg.post_only;
        let entry_price = self.slots[slot_idx].entry_price;
        let exit_price = self.slots[slot_idx].exit_price;
        let quantity = self.slots[slot_idx].quantity;
        let mut intents = Vec::new();

        if is_entry {
            let exit_side = match direction {
                Direction::Long => OrderSide::Sell,
                Direction::Short => OrderSide::Buy,
            };
            if Self::can_place_maker(exit_side, exit_price, best_bid, best_ask) {
                let next_cloid = self.next_client_order_id(slot_idx, false);
                self.slots[slot_idx].state = SlotState::ExitWorking {
                    client_order_id: next_cloid,
                };
                self.order_to_slot.insert(next_cloid, (slot_idx, false));
                intents.push(GridOrderIntent {
                    slot_index: slot_idx,
                    client_order_id: next_cloid,
                    side: exit_side,
                    price: exit_price,
                    quantity,
                    post_only,
                    reduce_only: true,
                    is_entry: false,
                });
            } else {
                self.slots[slot_idx].state = SlotState::HoldingWaitingExit;
            }
        } else {
            // Exit order filled -> completed one full grid arbitrage cycle!
            let spread = (exit_price - entry_price).abs();
            self.slots[slot_idx].completed_cycles += 1;
            self.slots[slot_idx].realized_gross_pnl_usdc += quantity * spread;

            let entry_side = match direction {
                Direction::Long => OrderSide::Buy,
                Direction::Short => OrderSide::Sell,
            };
            if Self::can_place_maker(entry_side, entry_price, best_bid, best_ask) {
                let next_cloid = self.next_client_order_id(slot_idx, true);
                self.slots[slot_idx].state = SlotState::EntryWorking {
                    client_order_id: next_cloid,
                };
                self.order_to_slot.insert(next_cloid, (slot_idx, true));
                intents.push(GridOrderIntent {
                    slot_index: slot_idx,
                    client_order_id: next_cloid,
                    side: entry_side,
                    price: entry_price,
                    quantity,
                    post_only,
                    reduce_only: false,
                    is_entry: true,
                });
            } else {
                self.slots[slot_idx].state = SlotState::Idle;
            }
        }

        intents
    }

    /// Called when an order is rejected (e.g., Post-Only ALO crossing reject) or canceled before filling.
    /// Reverts an Entry order slot to `Idle` and an Exit order slot to `HoldingWaitingExit` so it will
    /// be retried on the next valid market tick.
    pub fn on_order_terminal_unfilled(&mut self, client_order_id: &ClientOrderId) {
        let Some((slot_idx, is_entry)) = self.order_to_slot.remove(client_order_id) else {
            return;
        };
        if is_entry {
            self.slots[slot_idx].state = SlotState::Idle;
        } else {
            self.slots[slot_idx].state = SlotState::HoldingWaitingExit;
        }
    }

    /// Computes a full snapshot of grid positions, active Maker orders, and realized/unrealized PnL.
    pub fn summary_metrics(&self, current_price: f64) -> GridSummary {
        let mut active_entry_orders = 0;
        let mut active_exit_orders = 0;
        let mut holding_slots = 0;
        let mut completed_cycles = 0;
        let mut realized_gross_pnl_usdc = 0.0;
        let mut unrealized_pnl_usdc = 0.0;
        let mut total_working_entry_notional_usdc = 0.0;
        let mut total_position_base_qty = 0.0;
        let mut total_position_cost_usdc = 0.0;

        for slot in &self.slots {
            completed_cycles += slot.completed_cycles;
            realized_gross_pnl_usdc += slot.realized_gross_pnl_usdc;

            match slot.state {
                SlotState::Idle => {}
                SlotState::EntryWorking { .. } => {
                    active_entry_orders += 1;
                    total_working_entry_notional_usdc += slot.quantity * slot.entry_price;
                }
                SlotState::HoldingWaitingExit | SlotState::ExitWorking { .. } => {
                    if matches!(slot.state, SlotState::ExitWorking { .. }) {
                        active_exit_orders += 1;
                    }
                    holding_slots += 1;
                    total_position_base_qty += slot.quantity;
                    total_position_cost_usdc += slot.quantity * slot.entry_price;
                    let pnl_per_unit = match self.cfg.direction {
                        Direction::Long => current_price - slot.entry_price,
                        Direction::Short => slot.entry_price - current_price,
                    };
                    unrealized_pnl_usdc += slot.quantity * pnl_per_unit;
                }
            }
        }

        GridSummary {
            total_grids: self.slots.len(),
            active_entry_orders,
            active_exit_orders,
            holding_slots,
            completed_cycles,
            realized_gross_pnl_usdc,
            unrealized_pnl_usdc,
            total_working_entry_notional_usdc,
            total_position_base_qty,
            total_position_cost_usdc,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hype_long_config() -> GridConfig {
        GridConfig {
            symbol: "HYPE".to_string(),
            direction: Direction::Long,
            lower_price: 55.0,
            upper_price: 65.0,
            grid_count: 30,
            amount_per_grid: 1000.0,
            post_only: true,
            follow_price_into_idle_grids: true,
        }
    }

    #[test]
    fn startup_at_60_12_places_16_maker_buy_orders_below_market_and_zero_initial_position() {
        let mut engine = GridEngine::new(sample_hype_long_config(), 2).expect("valid engine");
        assert_eq!(engine.slots().len(), 30);

        // Current market price 60.12 (best_bid = 60.11, best_ask = 60.13)
        let intents = engine.on_market_price(60.11, 60.13);

        // Levels below 60.12 are P_0 (55.0) through P_15 (60.0) -> 16 Buy orders
        assert_eq!(intents.len(), 16);
        assert!(intents.iter().all(|o| o.side == OrderSide::Buy));
        assert!(intents.iter().all(|o| o.post_only));
        assert!(intents.iter().all(|o| !o.reduce_only));
        assert!((intents[0].price - 55.0).abs() < 1e-6);
        assert!((intents[15].price - 60.0).abs() < 1e-6);

        let summary = engine.summary_metrics(60.12);
        assert_eq!(summary.active_entry_orders, 16);
        assert_eq!(summary.holding_slots, 0);
        assert!((summary.total_position_base_qty - 0.0).abs() < 1e-9);
    }

    #[test]
    fn buy_fill_places_maker_reduce_only_sell_one_grid_higher_and_sell_fill_rearms_buy() {
        let mut engine = GridEngine::new(sample_hype_long_config(), 2).expect("valid engine");
        let intents = engine.on_market_price(60.11, 60.13);
        let top_buy = intents.last().expect("slot 15 buy at 60.0").clone();
        assert_eq!(top_buy.slot_index, 15);
        assert!((top_buy.price - 60.0).abs() < 1e-6);

        // Price dips to 59.95 / 59.98 and fills slot 15 buy order at 60.0
        let exit_intents = engine.on_order_filled(&top_buy.client_order_id, 59.95, 59.98);
        assert_eq!(exit_intents.len(), 1);
        let sell_order = &exit_intents[0];
        assert_eq!(sell_order.slot_index, 15);
        assert_eq!(sell_order.side, OrderSide::Sell);
        assert!(sell_order.post_only);
        assert!(sell_order.reduce_only);
        // Exit price for slot 15 is 60.333
        assert!((sell_order.price - 60.333).abs() < 1e-3);
        assert!((sell_order.quantity - top_buy.quantity).abs() < 1e-9);

        // Now price rallies to 60.35 / 60.37 and fills the Sell order at 60.333
        let rearm_intents = engine.on_order_filled(&sell_order.client_order_id, 60.35, 60.37);
        assert_eq!(rearm_intents.len(), 1);
        let rearmed_buy = &rearm_intents[0];
        assert_eq!(rearmed_buy.slot_index, 15);
        assert_eq!(rearmed_buy.side, OrderSide::Buy);
        assert!(rearmed_buy.post_only);
        assert!(!rearmed_buy.reduce_only);
        assert!((rearmed_buy.price - 60.0).abs() < 1e-6);

        let summary = engine.summary_metrics(60.36);
        assert_eq!(summary.completed_cycles, 1);
        // Gross profit for 16.66 HYPE * 0.333 USDC = ~5.54778 USDC
        assert!(summary.realized_gross_pnl_usdc > 5.50 && summary.realized_gross_pnl_usdc < 5.60);
    }

    #[test]
    fn drop_to_55_00_accumulates_expected_position_and_unrealized_loss() {
        let mut engine = GridEngine::new(sample_hype_long_config(), 2).expect("valid engine");
        let intents = engine.on_market_price(60.11, 60.13);
        assert_eq!(intents.len(), 16);

        // Simulate price dropping all the way to 55.00, filling all 16 Buy orders
        for intent in &intents {
            let _ = engine.on_order_filled(&intent.client_order_id, 54.98, 55.00);
        }

        let summary = engine.summary_metrics(55.00);
        assert_eq!(summary.holding_slots, 16);
        // Total base quantity ~278.34 HYPE, unrealized PnL at 55.00 ~ -687 USDC
        assert!(summary.total_position_base_qty > 278.0 && summary.total_position_base_qty < 279.0);
        assert!(summary.unrealized_pnl_usdc < -685.0 && summary.unrealized_pnl_usdc > -690.0);
    }
}
