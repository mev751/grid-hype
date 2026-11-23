use anyhow::{Context, Result, bail};
use ethers::signers::LocalWallet;
use hyperliquid_rust_sdk::{
    BaseUrl, ClientCancelRequest, ClientLimit, ClientOrder, ClientOrderRequest,
    ExchangeClient, ExchangeResponseStatus,
};
use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Deserialize)]
pub struct L2Level {
    pub px: String,
    pub sz: String,
    pub n: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct L2BookResponse {
    pub coin: String,
    pub time: u64,
    pub levels: Vec<Vec<L2Level>>,
}

#[derive(Debug, Clone)]
pub struct HyperliquidClient {
    http_client: HttpClient,
    info_url: String,
    exchange_client: Option<ExchangeClient>,
    wallet_address: Option<String>,
}

impl HyperliquidClient {
    pub async fn new(
        testnet: bool,
        private_key: Option<&str>,
        account_address: Option<&str>,
    ) -> Result<Self> {
        let http_client = HttpClient::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        let (info_url, base_url) = if testnet {
            ("https://api.hyperliquid-testnet.xyz/info".to_string(), BaseUrl::Testnet)
        } else {
            ("https://api.hyperliquid.xyz/info".to_string(), BaseUrl::Mainnet)
        };

        let mut exchange_client = None;
        let mut wallet_address = None;

        if let Some(pk) = private_key {
            let pk_clean = pk.trim().trim_start_matches("0x");
            let wallet: LocalWallet = pk_clean
                .parse()
                .context("Failed to parse private key into LocalWallet")?;
            wallet_address = Some(format!("{:?}", wallet.address()));

            let vault_addr = account_address.and_then(|addr| addr.parse().ok());
            let client = ExchangeClient::new(
                Some(http_client.clone()),
                wallet,
                Some(base_url),
                None,
                vault_addr,
            )
            .await
            .context("Failed to initialize Hyperliquid ExchangeClient")?;

            exchange_client = Some(client);
        }

        Ok(Self {
            http_client,
            info_url,
            exchange_client,
            wallet_address,
        })
    }

    pub fn wallet_address(&self) -> Option<&str> {
        self.wallet_address.as_deref()
    }

    pub async fn get_bbo(&self, coin: &str) -> Result<(f64, f64)> {
        let payload = json!({
            "type": "l2Book",
            "coin": coin,
        });

        let res = self
            .http_client
            .post(&self.info_url)
            .json(&payload)
            .send()
            .await
            .context("Failed to send l2Book request to Hyperliquid")?;

        let text = res.text().await?;
        let book: L2BookResponse = serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse L2 book JSON: {}", text))?;

        let bids = book.levels.first().and_then(|lvl| lvl.first());
        let asks = book.levels.get(1).and_then(|lvl| lvl.first());

        let best_bid = bids
            .and_then(|b| b.px.parse::<f64>().ok())
            .context("Empty or invalid best bid in L2 book")?;
        let best_ask = asks
            .and_then(|a| a.px.parse::<f64>().ok())
            .context("Empty or invalid best ask in L2 book")?;

        if best_bid <= 0.0 || best_ask <= 0.0 {
            bail!("Invalid BBO prices: bid={}, ask={}", best_bid, best_ask);
        }

        Ok((best_bid, best_ask))
    }

    pub async fn get_account_balances(&self, user_address: &str) -> (f64, f64) {
        let mut perp_margin = 0.0;
        let mut spot_usdc = 0.0;

        // 1. Perp clearinghouse state
        let payload = json!({
            "type": "clearinghouseState",
            "user": user_address,
        });
        if let Ok(res) = self.http_client.post(&self.info_url).json(&payload).send().await {
            if let Ok(val) = res.json::<Value>().await {
                if let Some(account_val) = val
                    .get("marginSummary")
                    .and_then(|m| m.get("accountValue"))
                    .and_then(|v| v.as_str())
                {
                    perp_margin = account_val.parse::<f64>().unwrap_or(0.0);
                }
            }
        }

        // 2. Spot clearinghouse state
        let spot_payload = json!({
            "type": "spotClearinghouseState",
            "user": user_address,
        });
        if let Ok(res) = self.http_client.post(&self.info_url).json(&spot_payload).send().await {
            if let Ok(val) = res.json::<Value>().await {
                if let Some(balances) = val.get("balances").and_then(|b| b.as_array()) {
                    for b in balances {
                        if b.get("coin").and_then(|c| c.as_str()) == Some("USDC") {
                            if let Some(total_str) = b.get("total").and_then(|t| t.as_str()) {
                                spot_usdc = total_str.parse::<f64>().unwrap_or(0.0);
                            }
                        }
                    }
                }
            }
        }

        (perp_margin, spot_usdc)
    }

    pub async fn get_open_orders(&self, user_address: &str) -> Result<Vec<Value>> {
        let payload = json!({
            "type": "openOrders",
            "user": user_address,
        });
        let res = self
            .http_client
            .post(&self.info_url)
            .json(&payload)
            .send()
            .await?
            .json::<Vec<Value>>()
            .await?;
        Ok(res)
    }

    pub async fn place_maker_order(
        &self,
        asset: &str,
        is_buy: bool,
        price: f64,
        size: f64,
        reduce_only: bool,
        post_only: bool,
    ) -> Result<u64> {
        let client = self
            .exchange_client
            .as_ref()
            .context("ExchangeClient not initialized (private key missing)")?;

        let tif = if post_only { "Alo" } else { "Gtc" };
        let req = ClientOrderRequest {
            asset: asset.to_string(),
            is_buy,
            limit_px: price,
            sz: size,
            reduce_only,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: tif.to_string(),
            }),
            cloid: None,
        };

        let response = client.order(req, None).await;
        match response {
            Ok(ExchangeResponseStatus::Ok(order_res)) => {
                debug!(?order_res, "Order response from Hyperliquid");
                // Extract order ID (oid) from response statuses
                let statuses_val = serde_json::to_value(&order_res)?;
                if let Some(statuses) = statuses_val.get("statuses").and_then(|s| s.as_array()) {
                    for status in statuses {
                        if let Some(resting) = status.get("resting") {
                            if let Some(oid) = resting.get("oid").and_then(|o| o.as_u64()) {
                                return Ok(oid);
                            }
                        }
                        if let Some(filled) = status.get("filled") {
                            if let Some(oid) = filled.get("oid").and_then(|o| o.as_u64()) {
                                return Ok(oid);
                            }
                        }
                        if let Some(err_msg) = status.get("error").and_then(|e| e.as_str()) {
                            bail!("Order placement rejected by venue: {}", err_msg);
                        }
                    }
                }
                bail!("Could not parse resting/filled order ID from response: {:?}", statuses_val);
            }
            Ok(ExchangeResponseStatus::Err(err_msg)) => {
                bail!("Hyperliquid API error: {}", err_msg);
            }
            Err(e) => {
                bail!("Hyperliquid network/signing error: {}", e);
            }
        }
    }

    pub async fn cancel_order(&self, asset: &str, oid: u64) -> Result<()> {
        let client = self
            .exchange_client
            .as_ref()
            .context("ExchangeClient not initialized (private key missing)")?;

        let req = ClientCancelRequest {
            asset: asset.to_string(),
            oid,
        };

        let response = client.cancel(req, None).await;
        match response {
            Ok(ExchangeResponseStatus::Ok(_)) => Ok(()),
            Ok(ExchangeResponseStatus::Err(err_msg)) => {
                warn!(oid, err = %err_msg, "Cancel order returned error status");
                Ok(())
            }
            Err(e) => {
                warn!(oid, err = %e, "Cancel order network error");
                Ok(())
            }
        }
    }
}
