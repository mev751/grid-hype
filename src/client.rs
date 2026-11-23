use anyhow::{Context, Result, bail};
use ethers::signers::{LocalWallet, Signer};
use hyperliquid_rust_sdk::{
    BaseUrl, ClientCancelRequest, ClientLimit, ClientOrder, ClientOrderRequest,
    ExchangeClient, ExchangeDataStatus, ExchangeResponseStatus,
};
use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tracing::{debug, warn};

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
    exchange_client: Option<Arc<ExchangeClient>>,
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

            exchange_client = Some(Arc::new(client));
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

        Ok((best_bid, best_ask))
    }

    pub async fn get_account_state(&self, user_address: &str) -> Result<(f64, Vec<(String, f64)>)> {
        let payload = json!({
            "type": "clearinghouseState",
            "user": user_address,
        });

        let res = self
            .http_client
            .post(&self.info_url)
            .json(&payload)
            .send()
            .await
            .context("Failed to send clearinghouseState request")?;

        let text = res.text().await?;
        let val: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse clearinghouseState JSON: {}", text))?;

        let margin_available = val
            .get("withdrawable")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        let mut positions = Vec::new();
        if let Some(pos_arr) = val.get("assetPositions").and_then(|v| v.as_array()) {
            for item in pos_arr {
                if let Some(pos) = item.get("position") {
                    let coin = pos.get("coin").and_then(|c| c.as_str()).unwrap_or("");
                    let szi = pos
                        .get("szi")
                        .and_then(|s| s.as_str())
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    if szi.abs() > 1e-6 {
                        positions.push((coin.to_string(), szi));
                    }
                }
            }
        }

        Ok((margin_available, positions))
    }

    pub async fn get_open_orders(&self, user_address: &str) -> Result<Vec<u64>> {
        let payload = json!({
            "type": "openOrders",
            "user": user_address,
        });

        let res = self
            .http_client
            .post(&self.info_url)
            .json(&payload)
            .send()
            .await
            .context("Failed to query openOrders")?;

        let text = res.text().await?;
        let val: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse openOrders JSON: {}", text))?;

        let mut oids = Vec::new();
        if let Some(orders) = val.as_array() {
            for ord in orders {
                if let Some(oid) = ord.get("oid").and_then(|o| o.as_u64()) {
                    oids.push(oid);
                }
            }
        }

        Ok(oids)
    }

    pub async fn place_order(
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
                if let Some(data) = order_res.data {
                    if !data.statuses.is_empty() {
                        match &data.statuses[0] {
                            ExchangeDataStatus::Resting(r) => return Ok(r.oid),
                            ExchangeDataStatus::Filled(f) => return Ok(f.oid),
                            ExchangeDataStatus::Error(e) => {
                                bail!("Order placement rejected by venue: {}", e);
                            }
                            other => {
                                bail!("Unexpected order status from venue: {:?}", other);
                            }
                        }
                    }
                }
                bail!("Empty statuses array in order response: {:?}", order_res);
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
            Ok(ExchangeResponseStatus::Ok(cancel_res)) => {
                if let Some(data) = cancel_res.data {
                    if !data.statuses.is_empty() {
                        if let ExchangeDataStatus::Error(e) = &data.statuses[0] {
                            warn!(oid, err = %e, "Cancel order returned error status");
                        }
                    }
                }
                Ok(())
            }
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
