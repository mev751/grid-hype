# Grid-HYPE (NautilusTrader Rust Hyperliquid Grid Arbitrage Engine) Design Spec

## 1. Intent & Why
Build a high-performance, generalized grid arbitrage trading engine in Rust based on `nautilus_trader` (`nautilus-hyperliquid`, `nautilus-model`, `nautilus-core`), targeting Hyperliquid Perpetuals (defaulting to `HYPE-PERP` / `HYPE`). Both entry and exit orders use Maker (`post_only = true` / `ALO`) to save fees. All grid parameters (`symbol`, `direction`, `lower_price`, `upper_price`, `grid_count`, `amount_per_grid`, etc.) are configured via `config.toml`, while the EVM private key is imported strictly via environment variables (`HYPERLIQUID_PK` or `PRIVATE_KEY`). The repository is pushed to `git@github.com:mev751/grid-hype.git`, built for `linux-amd64` (`x86_64-unknown-linux-gnu`) via GitHub Actions, and deployed to `box:/home/emc/grid-hype`.

## 2. Goals & Non-Goals
- **Goals:**
  - Native Rust implementation built on official `nautilus-hyperliquid` (`0.68.0`), `nautilus-model` (`0.68.0`), and `nautilus-core` (`0.68.0`).
  - Generalized grid engine configurable via `config.toml` supporting `long` and `short` directions, price range (`lower_price`..`upper_price`), `grid_count` (30 grids = 31 price levels), and `amount_per_grid` (1000 USDC notional per grid).
  - Maker-only (`post_only = true`) limit orders for both opening and closing (`reduce_only = true` on exit orders) positions.
  - Initial grid placement: for `long`, only place Maker Buy orders on grid levels strictly below the current market price (`P_i < current_price`); when a Buy order at level `i` fills, place a Maker Sell (`reduce_only`) order at level `i + 1`; when the Sell order at level `i + 1` fills, re-arm level `i` with a Maker Buy order. Dynamic follow: if price moves above an Idle level `i` (`P_i < best_bid`) that has no active position, place its entry Buy order.
  - Hyperliquid tick size (5 significant figures, max `6 - sz_decimals` decimal places for perps) and lot size (`sz_decimals`) precision rounding.
  - Private key loaded from environment variable (`HYPERLIQUID_PK` or `PRIVATE_KEY`, optional `HYPERLIQUID_VAULT`).
  - GitHub Actions workflow `.github/workflows/build.yml` compiling `x86_64-unknown-linux-gnu` release binary and deploying to `box:/home/emc/grid-hype`.
- **Non-Goals:**
  - Market orders or taker execution.
  - Hardcoding credentials in source or config files.

## 3. Approaches Considered
- **Approach 1 (Chosen): Direct `nautilus-hyperliquid` + `nautilus-model` Rust Engine**
  - Uses `HyperliquidHttpClient` (for EIP-712 signing, instrument metadata loading, L2 book queries, and open order reconciliation) and `HyperliquidWebSocketClient` (for real-time quote/book streaming, `orderUpdates`, `userFills`, and low-latency WS order submission/cancellation).
  - Pros: Pure single-file Rust binary, fast cross-compilation on GitHub Actions, deterministic state machine with unit-testable grid math and state transitions.
- **Approach 2: Python + PyO3 `LiveNode` wrapper**
  - Rejected because the user requested a pure Rust program compiled to a standalone Linux amd64 binary.

## 4. Architecture & Component Design
1. **`src/config.rs`**:
   - Parses `config.toml` into `AppConfig` (`GridConfig`, `ExchangeConfig`).
   - Validates `lower_price < upper_price`, `grid_count >= 1`, `amount_per_grid > 0.0`, `direction` in `["long", "short"]`.
   - Reads private key from env var specified by `private_key_env` (default `"HYPERLIQUID_PK"`, fallback `"PRIVATE_KEY"`).
2. **`src/precision.rs`**:
   - Hyperliquid perpetual price & size formatting:
     - Size (`sz`): rounded to `sz_decimals` decimal places.
     - Price (`px`): rounded to at most 5 significant digits AND at most `(6 - sz_decimals)` decimal places, ensuring Maker orders never violate Hyperliquid tick rules.
3. **`src/grid.rs`**:
   - Computes `grid_count + 1` price levels $P_0, P_1, \dots, P_N$ where $P_i = P_{\text{low}} + i \cdot \frac{P_{\text{high}} - P_{\text{low}}}{N}$.
   - Tracks `GridSlot` for each grid interval $i \in [0, N-1]$:
     - Entry price, exit price, quantity $Q_i = \text{round\_size}(\text{amount\_per\_grid} / P_{\text{entry}}, \text{sz\_decimals})$.
     - State machine: `Idle` -> `EntryWorking { cloid }` -> `ExitWorking { cloid, qty }` -> `Idle` (and immediately re-places entry when price allows Maker).
   - Pure state transition methods (`plan_initial_actions`, `on_price_tick`, `on_order_filled`, `on_order_rejected_or_canceled`) returning `Vec<GridAction>` (`PlaceOrder`, `CancelOrder`).
4. **`src/engine.rs` & `src/main.rs`**:
   - Connects `HyperliquidHttpClient` and `HyperliquidWebSocketClient` from `nautilus_hyperliquid`.
   - Loads instrument definition for `<symbol>-PERP` (e.g., `HYPE-PERP`), caches instrument in both HTTP and WS clients, extracts `sz_decimals`.
   - Fetches initial midpoint price via `info_l2_book`, subscribes to quotes/book and user order updates/fills via WebSocket.
   - Executes `GridAction`s via `ws_client.submit_order(&http_client, ...)` with `post_only = true` and `TimeInForce::Gtc`.
   - Supports `--dry-run` flag to verify live market data & grid calculations without placing real orders, and graceful shutdown (`Ctrl+C`) with optional cancel-on-exit.
