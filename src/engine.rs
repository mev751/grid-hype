use crate::{
    client::HyperliquidClient,
    config::AppConfig,
    grid::{GridActionPlan, GridEngine, GridOrderIntent, SlotState},
    precision::{format_hl_price, format_hl_size},
};
use anyhow::{Context, Result};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use tokio::{signal, time};
use tracing::{error, info, warn};

pub struct LiveGridRunner {
    cfg: AppConfig,
    dry_run: bool,
}

impl LiveGridRunner {
    pub fn new(cfg: AppConfig, dry_run: bool) -> Self {
        Self { cfg, dry_run }
    }

    pub async fn run(&self) -> Result<()> {
        let symbol = &self.cfg.grid.symbol;
        let sz_decimals = 2; // HYPE perpetual uses 2 decimals for size

        info!(
            symbol = %symbol,
            direction = ?self.cfg.grid.direction,
            lower_price = self.cfg.grid.lower_price,
            upper_price = self.cfg.grid.upper_price,
            grid_count = self.cfg.grid.grid_count,
            amount_per_grid = self.cfg.grid.amount_per_grid,
            post_only = self.cfg.grid.post_only,
            active_entry_grids = self.cfg.grid.active_entry_grids,
            testnet = self.cfg.exchange.testnet,
            dry_run = self.dry_run,
            "Starting Hyperliquid Generic Maker Grid Bot"
        );

        let mut grid = GridEngine::new(self.cfg.grid.clone(), sz_decimals)
            .context("Failed to initialize GridEngine")?;

        let private_key = if self.dry_run {
            self.cfg.resolve_private_key().ok()
        } else {
            Some(self.cfg.resolve_private_key().context("Private key required for live trading")?)
        };

        let account_address = self.cfg.resolve_account_address();

        let client = HyperliquidClient::new(
            self.cfg.exchange.testnet,
            private_key.as_deref(),
            account_address.as_deref(),
        )
        .await
        .context("Failed to initialize Hyperliquid client")?;

        // 1. Fetch initial BBO price
        let (best_bid, best_ask) = client.get_bbo(symbol).await?;
        let mid_price = (best_bid + best_ask) * 0.5;
        info!(best_bid, best_ask, mid_price, "Fetched live market BBO");

        let initial_plan = grid.on_market_price(best_bid, best_ask);
        let summary = grid.summary_metrics(mid_price);

        if self.dry_run {
            println!("==================== DRY-RUN GRID TABLE ====================");
            println!("Symbol: {} | Current BBO: bid={:.4}, ask={:.4}, mid={:.4}", symbol, best_bid, best_ask, mid_price);
            println!(
                "Direction: {:?} | Range: [{}, {}] | Grids: {} | Per Grid: {} USDC | Active Window: {} grids",
                self.cfg.grid.direction,
                self.cfg.grid.lower_price,
                self.cfg.grid.upper_price,
                self.cfg.grid.grid_count,
                self.cfg.grid.amount_per_grid,
                if self.cfg.grid.active_entry_grids == 0 { "All (Unlimited)".to_string() } else { self.cfg.grid.active_entry_grids.to_string() }
            );
            println!("------------------------------------------------------------");
            for slot in grid.slots() {
                let status_str = match &slot.state {
                    SlotState::Idle => {
                        if slot.entry_price <= mid_price {
                            "IDLE (Below Active Window)"
                        } else {
                            "IDLE (Above Market)"
                        }
                    }
                    SlotState::EntryWorking { .. } => "MAKER ENTRY ARMED (Active Window)",
                    SlotState::EntryCanceling { .. } => "CANCELING ENTRY",
                    SlotState::HoldingWaitingExit => "HOLDING (WAIT EXIT)",
                    SlotState::ExitWorking { .. } => "MAKER EXIT ARMED",
                };
                let gross_profit = slot.quantity * (slot.exit_price - slot.entry_price).abs();
                println!(
                    "Grid #{:02} | Entry: {:>8} -> Exit: {:>8} | Qty: {:>8} {} | GrossProfit: {:>6.2} USDC | {}",
                    slot.index,
                    format_hl_price(slot.entry_price, sz_decimals),
                    format_hl_price(slot.exit_price, sz_decimals),
                    format_hl_size(slot.quantity, sz_decimals),
                    symbol,
                    gross_profit,
                    status_str
                );
            }
            println!("============================================================");
            return Ok(());
        }

        let user_address = account_address
            .or_else(|| client.wallet_address().map(|s| s.to_string()))
            .context("Could not determine user account address")?;
        info!(user_address = %user_address, "Resolved Hyperliquid trading user address");

        let (margin, positions) = client.get_account_state(&user_address).await?;
        info!(
            margin_usdc = margin,
            positions = ?positions,
            "Hyperliquid account balance & positions snapshot"
        );

        let mut slot_cooldowns: HashMap<usize, Instant> = HashMap::new();

        // 2. Submit initial active Maker entry orders
        info!("Submitting initial Maker grid orders...");
        self.dispatch_plan(&client, symbol, sz_decimals, &mut grid, &mut slot_cooldowns, initial_plan).await;

        let mut interval = time::interval(Duration::from_secs(self.cfg.exchange.reconcile_interval_secs.max(2)));
        let sigint = signal::ctrl_c();
        tokio::pin!(sigint);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Reconcile: fetch BBO and open orders
                    if let Ok((bid, ask)) = client.get_bbo(symbol).await {
                        let cur_mid = (bid + ask) * 0.5;

                        // Query venue open orders to detect fills
                        if let Ok(venue_orders) = client.get_open_orders(&user_address).await {
                            let open_oids: HashSet<u64> = venue_orders.into_iter().collect();

                            // Check all working slots in grid
                            let mut filled_oids = Vec::new();
                            for slot in grid.slots() {
                                match &slot.state {
                                    SlotState::EntryWorking { oid } => {
                                        if !open_oids.contains(oid) {
                                            filled_oids.push((*oid, true, slot.index));
                                        }
                                    }
                                    SlotState::ExitWorking { oid } => {
                                        if !open_oids.contains(oid) {
                                            filled_oids.push((*oid, false, slot.index));
                                        }
                                    }
                                    _ => {}
                                }
                            }

                            // Process fills
                            for (oid, is_entry, slot_idx) in filled_oids {
                                info!(oid, slot = slot_idx, role = if is_entry { "ENTRY" } else { "EXIT" }, "Grid Maker order FILLED");
                                let fill_plan = grid.on_order_filled(oid, bid, ask);
                                let metrics = grid.summary_metrics(cur_mid);
                                info!(
                                    completed_cycles = metrics.completed_cycles,
                                    realized_gross_pnl_usdc = format!("{:.4}", metrics.realized_gross_pnl_usdc),
                                    unrealized_pnl_usdc = format!("{:.4}", metrics.unrealized_pnl_usdc),
                                    holding_slots = metrics.holding_slots,
                                    active_entries = metrics.active_entry_orders,
                                    active_exits = metrics.active_exit_orders,
                                    "Grid metrics updated after fill"
                                );
                                if !fill_plan.is_empty() {
                                    self.dispatch_plan(&client, symbol, sz_decimals, &mut grid, &mut slot_cooldowns, fill_plan).await;
                                }
                            }
                        }

                        // Evaluate active window against current market prices
                        let plan = grid.on_market_price(bid, ask);
                        if !plan.is_empty() {
                            self.dispatch_plan(&client, symbol, sz_decimals, &mut grid, &mut slot_cooldowns, plan).await;
                        }
                    }
                }
                _ = &mut sigint => {
                    info!("Received shutdown signal (Ctrl+C)");
                    if self.cfg.exchange.cancel_on_exit {
                        info!("Canceling all working grid orders on exit...");
                        for slot in grid.slots() {
                            match &slot.state {
                                SlotState::EntryWorking { oid }
                                | SlotState::EntryCanceling { oid }
                                | SlotState::ExitWorking { oid } => {
                                    let _ = client.cancel_order(symbol, *oid).await;
                                }
                                _ => {}
                            }
                        }
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    async fn dispatch_plan(
        &self,
        client: &HyperliquidClient,
        symbol: &str,
        sz_decimals: u8,
        grid: &mut GridEngine,
        slot_cooldowns: &mut HashMap<usize, Instant>,
        plan: GridActionPlan,
    ) {
        // 1. Process cancels
        for (slot_idx, oid) in plan.orders_to_cancel {
            info!(slot = slot_idx, oid, "Canceling order that left active window");
            let _ = client.cancel_order(symbol, oid).await;
            grid.on_order_canceled(oid);
        }

        // 2. Process places
        for intent in plan.orders_to_place {
            if let Some(retry_at) = slot_cooldowns.get(&intent.slot_index) {
                if Instant::now() < *retry_at {
                    continue;
                }
            }

            let price_str = format_hl_price(intent.price, sz_decimals);
            let size_str = format_hl_size(intent.quantity, sz_decimals);
            let price = price_str.parse::<f64>().unwrap_or(intent.price);
            let size = size_str.parse::<f64>().unwrap_or(intent.quantity);

            info!(
                slot = intent.slot_index,
                side = if intent.is_buy { "Buy" } else { "Sell" },
                price = %price_str,
                quantity = %size_str,
                role = if intent.is_entry { "ENTRY" } else { "EXIT" },
                "Submitting Maker limit order"
            );

            let res = client
                .place_order(symbol, intent.is_buy, price, size, intent.reduce_only, intent.post_only)
                .await;

            match res {
                Ok(oid) => {
                    info!(slot = intent.slot_index, oid, "Maker order successfully placed on book");
                    grid.on_order_placed(intent.slot_index, oid, intent.is_entry);
                    slot_cooldowns.remove(&intent.slot_index);
                }
                Err(err) => {
                    warn!(
                        slot = intent.slot_index,
                        error = %err,
                        "Order rejected by venue (likely Post-Only crossing); cooling down for 5s"
                    );
                    slot_cooldowns.insert(intent.slot_index, Instant::now() + Duration::from_secs(5));
                }
            }
        }
    }
}
