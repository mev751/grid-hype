use crate::{
    config::AppConfig,
    grid::{GridEngine, GridOrderIntent, SlotState},
    precision::{format_hl_price, format_hl_size},
};
use anyhow::{Context, Result, bail};
use nautilus_hyperliquid::{
    common::{consts::ws_url, enums::HyperliquidEnvironment},
    http::client::HyperliquidHttpClient,
    websocket::{
        client::HyperliquidWebSocketClient,
        messages::{ExecutionReport, NautilusWsMessage},
    },
};
use nautilus_model::{
    enums::{OrderStatus, OrderType, TimeInForce},
    identifiers::{AccountId, ClientOrderId, InstrumentId},
    instruments::{Instrument, InstrumentAny},
    types::{Price, Quantity},
};
use nautilus_network::websocket::TransportBackend;
use rust_decimal::prelude::ToPrimitive;
use std::time::Duration;
use tokio::{pin, signal, time};
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
        let environment = if self.cfg.exchange.testnet {
            HyperliquidEnvironment::Testnet
        } else {
            HyperliquidEnvironment::Mainnet
        };

        info!(
            symbol = %self.cfg.grid.symbol,
            direction = ?self.cfg.grid.direction,
            lower_price = self.cfg.grid.lower_price,
            upper_price = self.cfg.grid.upper_price,
            grid_count = self.cfg.grid.grid_count,
            amount_per_grid = self.cfg.grid.amount_per_grid,
            post_only = self.cfg.grid.post_only,
            environment = ?environment,
            dry_run = self.dry_run,
            "Initializing NautilusTrader Hyperliquid Grid Arbitrage Engine"
        );

        let mut http_client = if self.dry_run {
            match self.cfg.resolve_private_key() {
                Ok(pk) => {
                    let vault = self.cfg.resolve_vault_address();
                    HyperliquidHttpClient::from_credentials(
                        &pk,
                        vault.as_deref(),
                        environment,
                        self.cfg.exchange.http_timeout_secs,
                        None,
                    )
                    .context("Failed to initialize authenticated HyperliquidHttpClient")?
                }
                Err(_) => {
                    info!("Dry-run mode without private key: using unauthenticated HTTP client");
                    HyperliquidHttpClient::new(
                        environment,
                        self.cfg.exchange.http_timeout_secs,
                        None,
                    )
                    .context("Failed to initialize unauthenticated HyperliquidHttpClient")?
                }
            }
        } else {
            let pk = self.cfg.resolve_private_key()?;
            let vault = self.cfg.resolve_vault_address();
            HyperliquidHttpClient::from_credentials(
                &pk,
                vault.as_deref(),
                environment,
                self.cfg.exchange.http_timeout_secs,
                None,
            )
            .context("Failed to initialize authenticated HyperliquidHttpClient")?
        };

        http_client.set_include_builder_attribution(self.cfg.exchange.include_builder_attribution);
        http_client.set_account_id(AccountId::from("HYPERLIQUID-001"));

        info!("Loading Hyperliquid instrument definitions via NautilusTrader adapter...");
        let instruments = http_client
            .request_instruments()
            .await
            .context("Failed to fetch instruments from Hyperliquid")?;

        for inst in &instruments {
            http_client.cache_instrument(inst);
        }

        let target_coin = self.cfg.grid.symbol.as_str();
        let target_perp_symbol = format!("{target_coin}-USD-PERP");
        let instrument = instruments
            .iter()
            .find(|i| {
                i.id().symbol.as_str() == target_perp_symbol
                    || (i.raw_symbol().as_str() == target_coin
                        && matches!(i, InstrumentAny::CryptoPerpetual(_)))
            })
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Perpetual instrument '{}' (or raw_symbol '{}') not found on Hyperliquid",
                    target_perp_symbol,
                    target_coin
                )
            })?;

        let instrument_id = instrument.id();
        let sz_decimals = instrument.size_precision();
        let price_decimals = instrument.price_precision();

        info!(
            instrument_id = %instrument_id,
            raw_symbol = %instrument.raw_symbol(),
            sz_decimals,
            price_decimals,
            "Resolved target perpetual instrument"
        );

        let mut grid = GridEngine::new(self.cfg.grid.clone(), sz_decimals)?;

        // Fetch initial L2 order book snapshot
        let book = http_client
            .info_l2_book(instrument.raw_symbol().as_str())
            .await
            .context("Failed to fetch initial L2 book from Hyperliquid")?;

        let (mut best_bid, mut best_ask) = extract_bbo(&book)?;
        let initial_mid = (best_bid + best_ask) * 0.5;

        info!(
            best_bid,
            best_ask,
            initial_mid,
            "Fetched initial Hyperliquid order book BBO"
        );

        let initial_intents = grid.on_market_price(best_bid, best_ask);
        let summary = grid.summary_metrics(initial_mid);
        info!(
            initial_orders_to_place = initial_intents.len(),
            total_working_notional_usdc = format!("{:.2}", summary.total_working_entry_notional_usdc),
            "Computed initial Maker grid order layout"
        );

        if self.dry_run {
            println!("==================== DRY-RUN GRID TABLE ====================");
            println!(
                "Instrument: {} (sz_decimals={}, price_decimals={})",
                instrument_id, sz_decimals, price_decimals
            );
            println!(
                "Current BBO: bid={:.4}, ask={:.4}, mid={:.4}",
                best_bid, best_ask, initial_mid
            );
            println!(
                "Direction: {:?} | Range: [{}, {}] | Grids: {} | Per Grid: {} USDC | Maker Only: {}",
                self.cfg.grid.direction,
                self.cfg.grid.lower_price,
                self.cfg.grid.upper_price,
                self.cfg.grid.grid_count,
                self.cfg.grid.amount_per_grid,
                self.cfg.grid.post_only
            );
            println!("------------------------------------------------------------");
            for slot in grid.slots() {
                let status_str = match &slot.state {
                    SlotState::Idle => "IDLE (Above Market)",
                    SlotState::EntryWorking { .. } => "MAKER ENTRY ARMED",
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
                    target_coin,
                    gross_profit,
                    status_str
                );
            }
            println!("============================================================");
            return Ok(());
        }

        let user_address = http_client
            .get_account_address()
            .context("Failed to resolve user/vault account address from private key")?;
        info!(account_address = %user_address, "Authenticated Hyperliquid trading account");

        // Initialize WebSocket client for low-latency market data & order execution
        let url = ws_url(environment).to_string();
        let mut ws_client = HyperliquidWebSocketClient::new(
            Some(url),
            environment,
            Some(AccountId::from("HYPERLIQUID-001")),
            TransportBackend::default(),
            None,
        );
        ws_client.cache_instruments(instruments);
        ws_client
            .connect()
            .await
            .context("Failed to connect HyperliquidWebSocketClient")?;

        ws_client
            .subscribe_quotes(instrument_id)
            .await
            .context("Failed to subscribe to BBO quotes")?;
        ws_client
            .subscribe_all_user_channels(&user_address)
            .await
            .context("Failed to subscribe to user orderUpdates and userEvents")?;

        info!("WebSocket streams active; placing initial Maker grid orders...");
        self.dispatch_intents(
            &ws_client,
            &http_client,
            instrument_id,
            sz_decimals,
            &mut grid,
            initial_intents,
        )
        .await;

        let mut reconcile_timer = time::interval(Duration::from_secs(
            self.cfg.exchange.reconcile_interval_secs.max(5),
        ));
        let sigint = signal::ctrl_c();
        pin!(sigint);

        loop {
            tokio::select! {
                maybe_msg = ws_client.next_event() => {
                    let Some(msg) = maybe_msg else {
                        warn!("WebSocket stream ended; exiting event loop");
                        break;
                    };
                    match msg {
                        NautilusWsMessage::Quote(quote) => {
                            if quote.instrument_id == instrument_id {
                                let bid = quote.bid_price.as_f64();
                                let ask = quote.ask_price.as_f64();
                                if bid > 0.0 && ask > 0.0 {
                                    best_bid = bid;
                                    best_ask = ask;
                                    let intents = grid.on_market_price(best_bid, best_ask);
                                    if !intents.is_empty() {
                                        self.dispatch_intents(
                                            &ws_client,
                                            &http_client,
                                            instrument_id,
                                            sz_decimals,
                                            &mut grid,
                                            intents,
                                        )
                                        .await;
                                    }
                                }
                            }
                        }
                        NautilusWsMessage::ExecutionReports(reports) => {
                            for report in reports {
                                match report {
                                    ExecutionReport::Order(order_report) => {
                                        if order_report.instrument_id != instrument_id {
                                            continue;
                                        }
                                        let Some(cloid) = order_report.client_order_id else {
                                            continue;
                                        };
                                        match order_report.order_status {
                                            OrderStatus::Filled => {
                                                info!(
                                                    client_order_id = %cloid,
                                                    venue_order_id = %order_report.venue_order_id,
                                                    side = ?order_report.order_side,
                                                    filled_qty = %order_report.filled_qty,
                                                    "Grid Maker order FILLED"
                                                );
                                                let next_intents = grid.on_order_filled(&cloid, best_bid, best_ask);
                                                let metrics = grid.summary_metrics((best_bid + best_ask) * 0.5);
                                                info!(
                                                    completed_cycles = metrics.completed_cycles,
                                                    realized_gross_pnl_usdc = format!("{:.4}", metrics.realized_gross_pnl_usdc),
                                                    unrealized_pnl_usdc = format!("{:.4}", metrics.unrealized_pnl_usdc),
                                                    holding_slots = metrics.holding_slots,
                                                    active_entries = metrics.active_entry_orders,
                                                    active_exits = metrics.active_exit_orders,
                                                    "Grid status updated after fill"
                                                );
                                                if !next_intents.is_empty() {
                                                    self.dispatch_intents(
                                                        &ws_client,
                                                        &http_client,
                                                        instrument_id,
                                                        sz_decimals,
                                                        &mut grid,
                                                        next_intents,
                                                    )
                                                    .await;
                                                }
                                            }
                                            OrderStatus::Canceled
                                            | OrderStatus::Rejected
                                            | OrderStatus::Expired => {
                                                warn!(
                                                    client_order_id = %cloid,
                                                    status = ?order_report.order_status,
                                                    reason = ?order_report.cancel_reason,
                                                    "Grid order terminated without full fill; resetting slot for retry"
                                                );
                                                grid.on_order_terminal_unfilled(&cloid);
                                            }
                                            _ => {}
                                        }
                                    }
                                    ExecutionReport::Fill(fill_report) => {
                                        info!(
                                            trade_id = %fill_report.trade_id,
                                            client_order_id = ?fill_report.client_order_id,
                                            side = ?fill_report.order_side,
                                            last_px = %fill_report.last_px,
                                            last_qty = %fill_report.last_qty,
                                            commission = %fill_report.commission,
                                            "Execution fill event received"
                                        );
                                    }
                                }
                            }
                        }
                        NautilusWsMessage::Reconnected => {
                            info!("WebSocket reconnected; refreshing L2 book and checking grid orders");
                            if let Ok(book) = http_client.info_l2_book(instrument.raw_symbol().as_str()).await
                                && let Ok((bid, ask)) = extract_bbo(&book)
                            {
                                best_bid = bid;
                                best_ask = ask;
                            }
                        }
                        NautilusWsMessage::Error(err) => {
                            warn!(error = %err, "WebSocket message error");
                        }
                        _ => {}
                    }
                }
                _ = reconcile_timer.tick() => {
                    if let Ok(book) = http_client.info_l2_book(instrument.raw_symbol().as_str()).await
                        && let Ok((bid, ask)) = extract_bbo(&book)
                    {
                        best_bid = bid;
                        best_ask = ask;
                        let intents = grid.on_market_price(best_bid, best_ask);
                        if !intents.is_empty() {
                            self.dispatch_intents(
                                &ws_client,
                                &http_client,
                                instrument_id,
                                sz_decimals,
                                &mut grid,
                                intents,
                            )
                            .await;
                        }
                    }
                }
                _ = &mut sigint => {
                    info!("Received shutdown signal (Ctrl+C)");
                    if self.cfg.exchange.cancel_on_exit {
                        self.cancel_all_working_orders(&ws_client, &http_client, instrument_id, &grid).await;
                    }
                    let _ = ws_client.disconnect().await;
                    break;
                }
            }
        }

        Ok(())
    }

    async fn dispatch_intents(
        &self,
        ws_client: &HyperliquidWebSocketClient,
        http_client: &HyperliquidHttpClient,
        instrument_id: InstrumentId,
        sz_decimals: u8,
        grid: &mut GridEngine,
        intents: Vec<GridOrderIntent>,
    ) {
        for intent in intents {
            let price_str = format_hl_price(intent.price, sz_decimals);
            let size_str = format_hl_size(intent.quantity, sz_decimals);

            let price = Price::from(price_str.as_str());
            let quantity = Quantity::from(size_str.as_str());

            info!(
                slot = intent.slot_index,
                client_order_id = %intent.client_order_id,
                side = ?intent.side,
                price = %price_str,
                quantity = %size_str,
                post_only = intent.post_only,
                reduce_only = intent.reduce_only,
                role = if intent.is_entry { "ENTRY" } else { "EXIT" },
                "Submitting Maker limit order"
            );

            let res = ws_client
                .submit_order(
                    http_client,
                    instrument_id,
                    intent.client_order_id,
                    intent.side,
                    OrderType::Limit,
                    quantity,
                    TimeInForce::Gtc,
                    Some(price),
                    None,
                    intent.post_only,
                    intent.reduce_only,
                )
                .await;

            match res {
                Ok(Some(report)) => {
                    if matches!(
                        report.order_status,
                        OrderStatus::Rejected | OrderStatus::Canceled
                    ) {
                        warn!(
                            slot = intent.slot_index,
                            client_order_id = %intent.client_order_id,
                            status = ?report.order_status,
                            "Order rejected immediately (likely Post-Only ALO crossing); reverting slot for next tick"
                        );
                        grid.on_order_terminal_unfilled(&intent.client_order_id);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    error!(
                        slot = intent.slot_index,
                        client_order_id = %intent.client_order_id,
                        error = %e,
                        "Order submission failed; reverting slot state to retry on next tick"
                    );
                    grid.on_order_terminal_unfilled(&intent.client_order_id);
                }
            }
        }
    }

    async fn cancel_all_working_orders(
        &self,
        ws_client: &HyperliquidWebSocketClient,
        http_client: &HyperliquidHttpClient,
        instrument_id: InstrumentId,
        grid: &GridEngine,
    ) {
        let mut cancels: Vec<(InstrumentId, ClientOrderId, Option<_>)> = Vec::new();
        for slot in grid.slots() {
            match &slot.state {
                SlotState::EntryWorking { client_order_id }
                | SlotState::ExitWorking { client_order_id } => {
                    cancels.push((instrument_id, *client_order_id, None));
                }
                _ => {}
            }
        }
        if cancels.is_empty() {
            return;
        }
        info!(count = cancels.len(), "Canceling working grid orders on exit...");
        if let Err(e) = ws_client.cancel_orders(http_client, &cancels).await {
            warn!(error = %e, "Failed to cancel working orders on exit");
        }
    }
}

fn extract_bbo(
    book: &nautilus_hyperliquid::http::models::HyperliquidL2Book,
) -> Result<(f64, f64)> {
    let best_bid = book
        .levels
        .first()
        .and_then(|bids| bids.first())
        .and_then(|lvl| lvl.px.to_f64())
        .ok_or_else(|| anyhow::anyhow!("Empty bid book in HyperliquidL2Book"))?;
    let best_ask = book
        .levels
        .get(1)
        .and_then(|asks| asks.first())
        .and_then(|lvl| lvl.px.to_f64())
        .ok_or_else(|| anyhow::anyhow!("Empty ask book in HyperliquidL2Book"))?;

    if best_bid <= 0.0 || best_ask <= 0.0 {
        bail!("Invalid BBO prices: bid={best_bid}, ask={best_ask}");
    }
    Ok((best_bid, best_ask))
}
