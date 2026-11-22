# Grid-HYPE: NautilusTrader Hyperliquid Maker Grid Arbitrage Engine (Rust)

基于 [NautilusTrader](https://github.com/nautechsystems/nautilus_trader) 官方 Rust 核心 (`nautilus-hyperliquid`, `nautilus-model`, `nautilus-core`) 开发的通用 Hyperliquid 永续合约网格套利程序。

## 核心特性
- **纯 Maker 挂单 (`Post-Only / ALO`)**：开仓与平仓均强制使用 `Post-Only` 限价单节省手续费，平仓单强制 `reduce_only = true`。
- **全参数 `config.toml` 自定义**：支持自定义币种（如 `HYPE`）、方向（`long` 做多 / `short` 做空）、价格区间（如 `55.0 ~ 65.0`）、网格数量（如 `30` 格）、每格名义金额（如 `1000.0` USDC）。
- **零初始市价建仓**：以做多 (`long`) 为例，启动时仅在当前盘口价下方挂出 Maker 买单；当某格买单成交后，自动在上一格挂出 Maker 平仓卖单；卖单成交后重新在下一格挂回 Maker 买单。
- **私钥环境变量安全导入**：通过环境变量 `HYPERLIQUID_PK`（或 `PRIVATE_KEY`）导入 EVM 私钥，可选 `HYPERLIQUID_VAULT` 配置 Vault/子账户地址。

## 快速开始

### 1. 配置 `config.toml`
```toml
[grid]
symbol = "HYPE"
direction = "long"
lower_price = 55.0
upper_price = 65.0
grid_count = 30
amount_per_grid = 1000.0
post_only = true
follow_price_into_idle_grids = true

[exchange]
testnet = false
private_key_env = "HYPERLIQUID_PK"
vault_address_env = "HYPERLIQUID_VAULT"
http_timeout_secs = 15
reconcile_interval_secs = 10
cancel_on_exit = false
include_builder_attribution = false
```

### 2. 演练模式 (Dry-Run)
无需下单，直接连接 Hyperliquid 拉取最新 `HYPE-USD-PERP` 合约精度、实时盘口 BBO 并打印 30 格完整挂单计划与单格利润表：
```bash
./grid-hype --config config.toml --dry-run
```

### 3. 实盘启动
```bash
export HYPERLIQUID_PK="0x你的私钥"
./grid-hype --config config.toml
```
