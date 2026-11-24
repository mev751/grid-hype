use crate::{
    config::{Direction, GridConfig},
    precision::{round_hl_price, round_hl_size},
};
use anyhow::{Result, bail};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotState {
    /// No active order or position for this grid interval.
    Idle,
    /// Opening Maker order is resting on the book.
    EntryWorking { oid: u64 },
    /// Cancellation requested for entry order; still tracked until exchange confirms cancel or fill.
    EntryCanceling { oid: u64 },
    /// Entry order filled, waiting for market price to allow placing a valid Post-Only exit order.
    HoldingWaitingExit,
    /// Closing Maker (`reduce_only`) order is resting on the book.
    ExitWorking { oid: u64 },
}

#[derive(Debug, Clone)]
pub struct GridSlot {
    pub index: usize,
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
    pub is_buy: bool,
    pub price: f64,
    pub quantity: f64,
    pub post_only: bool,
    pub reduce_only: bool,
    pub is_entry: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GridActionPlan {
    pub orders_to_place: Vec<GridOrderIntent>,
    pub orders_to_cancel: Vec<(usize, u64)>, // (slot_index, oid)
}

impl GridActionPlan {
    pub fn empty() -> Self {
        Self {
            orders_to_place: Vec::new(),
            orders_to_cancel: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.orders_to_place.is_empty() && self.orders_to_cancel.is_empty()
    }
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
    oid_to_slot: HashMap<u64, (usize, bool)>, // oid -> (slot_index, is_entry)
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
            let (entry_raw, exit_raw) = match cfg.direction {
                Direction::Long => {
                    let entry = cfg.lower_price + (i as f64) * step;
                    let exit = entry + step;
                    (entry, exit)
                }
                Direction::Short => {
                    let entry = cfg.upper_price - (i as f64) * step;
                    let exit = entry - step;
                    (entry, exit)
                }
            };

            let entry_price = round_hl_price(entry_raw, sz_decimals);
            let exit_price = round_hl_price(exit_raw, sz_decimals);
            let quantity = round_hl_size(cfg.amount_per_grid / entry_price, sz_decimals);

            slots.push(GridSlot {
                index: i,
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
            oid_to_slot: HashMap::new(),
        })
    }

    pub fn slots(&self) -> &[GridSlot] {
        &self.slots
    }

    pub fn slots_mut(&mut self) -> &mut [GridSlot] {
        &mut self.slots
    }

    pub fn adopt_order(&mut self, slot_index: usize, oid: u64, is_entry: bool) {
        if slot_index >= self.slots.len() {
            return;
        }
        self.oid_to_slot.insert(oid, (slot_index, is_entry));
        if is_entry {
            self.slots[slot_index].state = SlotState::EntryWorking { oid };
        } else {
            self.slots[slot_index].state = SlotState::ExitWorking { oid };
        }
    }

    /// Reconciles existing on-chain position on startup.
    /// Marks the matching idle slots as HoldingWaitingExit so exit orders will be armed.
    pub fn reconcile_existing_position(&mut self, total_qty: f64, mid_price: f64) {
        let mut remaining_qty = total_qty;
        for slot in self.slots.iter_mut().rev() {
            if slot.entry_price <= mid_price && slot.state == SlotState::Idle && remaining_qty >= slot.quantity * 0.5 {
                slot.state = SlotState::HoldingWaitingExit;
                remaining_qty -= slot.quantity;
                if remaining_qty <= 1e-4 {
                    break;
                }
            }
        }
    }

    pub fn config(&self) -> &GridConfig {
        &self.cfg
    }

    /// Checks whether an order at `price` with `is_buy` can be placed as a Maker order without immediate crossing.
    pub fn can_place_maker(is_buy: bool, price: f64, best_bid: f64, best_ask: f64) -> bool {
        if is_buy {
            price < best_ask
        } else {
            price > best_bid
        }
    }

    /// Evaluates market prices, manages active entry window, and emits orders to place and cancel.
    pub fn on_market_price(&mut self, best_bid: f64, best_ask: f64) -> GridActionPlan {
        if !best_bid.is_finite() || !best_ask.is_finite() || best_bid <= 0.0 || best_ask <= 0.0 {
            return GridActionPlan::empty();
        }
        let mid_price = (best_bid + best_ask) * 0.5;
        let mut plan = GridActionPlan::empty();
        let direction = self.cfg.direction;
        let post_only = self.cfg.post_only;
        let active_entry_limit = self.cfg.active_entry_grids;

        // 1. Evaluate any HoldingWaitingExit slots (exit orders waiting to be placed)
        for idx in 0..self.slots.len() {
            if matches!(self.slots[idx].state, SlotState::HoldingWaitingExit) {
                let is_buy = match direction {
                    Direction::Long => false,
                    Direction::Short => true,
                };
                let exit_price = self.slots[idx].exit_price;
                if Self::can_place_maker(is_buy, exit_price, best_bid, best_ask) {
                    plan.orders_to_place.push(GridOrderIntent {
                        slot_index: idx,
                        is_buy,
                        price: exit_price,
                        quantity: self.slots[idx].quantity,
                        post_only,
                        reduce_only: true,
                        is_entry: false,
                    });
                }
            }
        }

        // 2. Evaluate candidate entry slots
        let mut candidate_entry_indices: Vec<usize> = Vec::new();
        for idx in 0..self.slots.len() {
            let state = &self.slots[idx].state;
            if matches!(state, SlotState::HoldingWaitingExit | SlotState::ExitWorking { .. }) {
                continue;
            }

            let entry_price = self.slots[idx].entry_price;
            let is_below_market = match direction {
                Direction::Long => entry_price <= mid_price,
                Direction::Short => entry_price >= mid_price,
            };

            // An entry slot is eligible if it is below/above market price, or already has an entry order working/canceling
            if is_below_market || matches!(state, SlotState::EntryWorking { .. } | SlotState::EntryCanceling { .. }) {
                candidate_entry_indices.push(idx);
            }
        }

        // Sort candidate slots closest to current market price
        match direction {
            Direction::Long => candidate_entry_indices.sort_by(|&a, &b| {
                self.slots[b]
                    .entry_price
                    .partial_cmp(&self.slots[a].entry_price)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            Direction::Short => candidate_entry_indices.sort_by(|&a, &b| {
                self.slots[a]
                    .entry_price
                    .partial_cmp(&self.slots[b].entry_price)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
        }

        let target_slots: Vec<usize> = if active_entry_limit == 0 {
            candidate_entry_indices
        } else {
            candidate_entry_indices.into_iter().take(active_entry_limit).collect()
        };

        // Cancel any active EntryWorking slots that are no longer in target_slots (e.g. price moved up away from them)
        for idx in 0..self.slots.len() {
            if let SlotState::EntryWorking { oid } = self.slots[idx].state {
                if !target_slots.contains(&idx) {
                    plan.orders_to_cancel.push((idx, oid));
                    // Transition to EntryCanceling but KEEP oid mapping so if it fills in-flight, it's processed!
                    self.slots[idx].state = SlotState::EntryCanceling { oid };
                }
            }
        }

        // Arm any target slots that are currently Idle (only if can_place_maker is true)
        for &idx in &target_slots {
            if matches!(self.slots[idx].state, SlotState::Idle) {
                let is_buy = match direction {
                    Direction::Long => true,
                    Direction::Short => false,
                };
                let entry_price = self.slots[idx].entry_price;
                if Self::can_place_maker(is_buy, entry_price, best_bid, best_ask) {
                    plan.orders_to_place.push(GridOrderIntent {
                        slot_index: idx,
                        is_buy,
                        price: entry_price,
                        quantity: self.slots[idx].quantity,
                        post_only,
                        reduce_only: false,
                        is_entry: true,
                    });
                }
            }
        }

        plan
    }

    /// Registers the placed order's exchange ID (`oid`) to the slot.
    pub fn on_order_placed(&mut self, slot_index: usize, oid: u64, is_entry: bool) {
        if slot_index >= self.slots.len() {
            return;
        }
        self.oid_to_slot.insert(oid, (slot_index, is_entry));
        if is_entry {
            self.slots[slot_index].state = SlotState::EntryWorking { oid };
        } else {
            self.slots[slot_index].state = SlotState::ExitWorking { oid };
        }
    }

    /// Handles order fill event.
    /// - Entry filled -> enters HoldingWaitingExit, evaluates exit order + replenishes 1 lower entry order.
    /// - Exit filled -> completes cycle, records realized profit, resets to Idle, evaluates active window.
    pub fn on_order_filled(&mut self, oid: u64, best_bid: f64, best_ask: f64) -> GridActionPlan {
        let Some((slot_idx, is_entry)) = self.oid_to_slot.remove(&oid) else {
            return GridActionPlan::empty();
        };

        let entry_price = self.slots[slot_idx].entry_price;
        let exit_price = self.slots[slot_idx].exit_price;
        let quantity = self.slots[slot_idx].quantity;

        if is_entry {
            self.slots[slot_idx].state = SlotState::HoldingWaitingExit;
        } else {
            let spread = (exit_price - entry_price).abs();
            self.slots[slot_idx].completed_cycles += 1;
            self.slots[slot_idx].realized_gross_pnl_usdc += quantity * spread;
            self.slots[slot_idx].state = SlotState::Idle;
        }

        self.on_market_price(best_bid, best_ask)
    }

    /// Handles order canceled / rejected event.
    pub fn on_order_canceled(&mut self, oid: u64) {
        let Some((slot_idx, is_entry)) = self.oid_to_slot.remove(&oid) else {
            return;
        };
        if is_entry {
            self.slots[slot_idx].state = SlotState::Idle;
        } else {
            // Exit order canceled/rejected -> revert to HoldingWaitingExit to retry
            self.slots[slot_idx].state = SlotState::HoldingWaitingExit;
        }
    }

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
                SlotState::EntryWorking { .. } | SlotState::EntryCanceling { .. } => {
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
            active_entry_grids: 3,
        }
    }

    #[test]
    fn startup_at_63_09_places_3_maker_buy_orders_below_market() {
        let mut engine = GridEngine::new(sample_hype_long_config(), 2).expect("valid engine");
        assert_eq!(engine.slots().len(), 30);

        let plan = engine.on_market_price(63.08, 63.10);
        // Only 3 entry orders closest to 63.09 below market
        assert_eq!(plan.orders_to_place.len(), 3);
        assert!(plan.orders_to_cancel.is_empty());

        let top_order = &plan.orders_to_place[2];
        assert_eq!(top_order.slot_index, 24); // 55 + 24 * (10/30) = 63.00
        assert!((top_order.price - 63.00).abs() < 1e-3);
        assert!(top_order.is_buy);
        assert!(top_order.post_only);
        assert!(!top_order.reduce_only);

        // Register the orders
        for (i, intent) in plan.orders_to_place.iter().enumerate() {
            engine.on_order_placed(intent.slot_index, 1000 + i as u64, true);
        }

        let summary = engine.summary_metrics(63.09);
        assert_eq!(summary.active_entry_orders, 3);
        assert_eq!(summary.holding_slots, 0);
    }

    #[test]
    fn resting_entry_order_is_never_canceled_when_price_touches_it() {
        let mut engine = GridEngine::new(sample_hype_long_config(), 2).expect("valid engine");
        let plan = engine.on_market_price(63.08, 63.10);
        for (i, intent) in plan.orders_to_place.iter().enumerate() {
            engine.on_order_placed(intent.slot_index, 1000 + i as u64, true);
        }

        // Price dips and touches 63.00
        let tick_plan = engine.on_market_price(63.00, 63.01);
        assert!(tick_plan.orders_to_cancel.is_empty(), "Must not cancel resting entry order when price touches it");

        // Price dips further through 63.00 to 62.99
        let dip_plan = engine.on_market_price(62.99, 63.00);
        assert!(dip_plan.orders_to_cancel.is_empty(), "Must not cancel resting entry order when price drops below it");
    }

    #[test]
    fn entry_fill_immediately_places_exit_order_and_replenishes_next_lower_entry_grid() {
        let mut engine = GridEngine::new(sample_hype_long_config(), 2).expect("valid engine");
        let plan = engine.on_market_price(63.08, 63.10);
        // slots: 22 (62.333), 23 (62.667), 24 (63.00)
        let slot_24_intent = plan.orders_to_place.iter().find(|o| o.slot_index == 24).unwrap();
        let slot_24_oid = 9999;
        engine.on_order_placed(24, slot_24_oid, true);

        // Slot 24 fills at 63.00
        let fill_plan = engine.on_order_filled(slot_24_oid, 62.95, 62.98);

        // fill_plan must place Exit order for slot 24 (at 63.333) AND arm next lower entry grid (slot 21 at 62.00)!
        let has_exit_24 = fill_plan.orders_to_place.iter().any(|o| o.slot_index == 24 && !o.is_buy && o.reduce_only);
        let has_entry_21 = fill_plan.orders_to_place.iter().any(|o| o.slot_index == 21 && o.is_buy && !o.reduce_only);
        assert!(has_exit_24, "Must place Exit sell order for filled slot 24");
        assert!(has_entry_21, "Must replenish next lower entry grid (slot 21)");

        // Register the Exit order
        let exit_intent = fill_plan.orders_to_place.iter().find(|o| o.slot_index == 24).unwrap();
        let exit_oid = 8888;
        engine.on_order_placed(24, exit_oid, false);

        // Now price rallies and fills the Exit order at 63.333
        let rearm_plan = engine.on_order_filled(exit_oid, 63.35, 63.37);
        assert!(rearm_plan.orders_to_place.iter().any(|o| o.slot_index == 24 && o.is_buy));

        let summary = engine.summary_metrics(63.36);
        assert_eq!(summary.completed_cycles, 1);
        assert!(summary.realized_gross_pnl_usdc > 5.0);
    }

    #[test]
    fn startup_reconciliation_adopts_existing_orders_and_avoids_duplicate_placement() {
        let mut engine = GridEngine::new(sample_hype_long_config(), 2).expect("valid engine");
        
        // Suppose slot 24 (63.00) already had an order 12345 on the exchange before restart
        engine.adopt_order(24, 12345, true);

        // Run market evaluation at 63.08
        let plan = engine.on_market_price(63.08, 63.10);

        // Slot 24 MUST NOT be in orders_to_place!
        assert!(!plan.orders_to_place.iter().any(|o| o.slot_index == 24), "Adopted slot 24 must not place duplicate order");
        
        // Instead, only the remaining 2 candidate slots in the active window (23 and 22) should place orders
        assert_eq!(plan.orders_to_place.len(), 2);
    }

    #[test]
    fn startup_reconciliation_arms_exit_for_existing_unhedged_position() {
        let mut engine = GridEngine::new(sample_hype_long_config(), 2).expect("valid engine");
        
        // Account holds ~15.8 HYPE on startup (approx 1 slot at 63.00)
        engine.reconcile_existing_position(15.8, 63.05);

        assert_eq!(engine.slots()[24].state, SlotState::HoldingWaitingExit);

        // Run market evaluation at 63.08
        let plan = engine.on_market_price(63.08, 63.10);

        // Must place exit sell order for slot 24 at 63.333!
        let has_exit = plan.orders_to_place.iter().any(|o| o.slot_index == 24 && !o.is_buy && o.reduce_only);
        assert!(has_exit, "Must place exit order for reconciled holding position on startup");
    }
}
