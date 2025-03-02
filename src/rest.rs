use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use log::trace;
use reqwest::header::{HeaderMap, HeaderValue};
use starknet_core::types::Felt;
use starknet_core::utils::cairo_short_string_to_felt;
use starknet_signers::SigningKey;
use tokio::sync::RwLock;

use crate::error::{Error, Result};
use crate::message::{account_address, auth_headers, sign_order};
use crate::structs::{
    AccountInformation, Balances, JWTToken, MarketSummaryStatic, OrderRequest, OrderUpdate,
    Positions, ResultsContainer, SystemConfig, BBO,
};
use crate::url::URL;

const JWT_UPDATE_INTERVAL: u64 = 240;

enum Method {
    Get,
    Post,
    Delete,
}

pub struct Client {
    url: URL,
    client: reqwest::Client,
    l2_chain_private_key_account: Option<(Felt, SigningKey, Felt)>,
    jwt: Arc<RwLock<(SystemTime, String)>>, // the current valid JWT and timestamp created
}

impl Client {
    /// Create a new Client instance
    ///
    /// # Parameters
    ///
    /// * `url` - A URL struct representing the base URL for the REST API
    /// * `l2_private_key_hex_str` - An optional string representing the private key for the L2 chain
    ///
    /// # Returns
    ///
    /// A Result with the new Client instance
    ///
    /// # Errors
    ///
    /// If the client cannot be created
    pub async fn new(url: URL, l2_private_key_hex_str: Option<String>) -> Result<Self> {
        let mut new_client = Self {
            url,
            client: reqwest::Client::new(),
            l2_chain_private_key_account: None,
            jwt: Arc::new(RwLock::new((UNIX_EPOCH, "".to_string()))),
        };
        if let Some(hex_str) = l2_private_key_hex_str {
            let signing_key = SigningKey::from_secret_scalar(
                Felt::from_hex(hex_str.as_str())
                    .map_err(|e| Error::StarknetError(e.to_string()))?,
            );
            let public_key = signing_key.verifying_key();
            let system_config = new_client.system_config().await?;

            let account = account_address(
                public_key.scalar(),
                Felt::from_str(system_config.paraclear_account_proxy_hash.as_str())
                    .map_err(|e| Error::StarknetError(e.to_string()))?,
                Felt::from_str(system_config.paraclear_account_hash.as_str())
                    .map_err(|e| Error::StarknetError(e.to_string()))?,
            )
            .map_err(|e| Error::StarknetError(e.to_string()))?;

            let chain_id = cairo_short_string_to_felt(system_config.starknet_chain_id.as_str())
                .map_err(|e| Error::StarknetError(e.to_string()))?;

            new_client.l2_chain_private_key_account = Some((chain_id, signing_key, account));
        }
        Ok(new_client)
    }

    /// Get the Paradex system configuration
    ///
    /// # Returns
    ///
    /// A SystemConfig struct representing the system configuration
    ///
    /// # Errors
    ///
    /// If the system configuration cannot be retrieved
    pub async fn system_config(&self) -> Result<SystemConfig> {
        self.request(Method::Get, "/v1/system/config".into(), None::<()>, None)
            .await
    }

    /// Get the list of markets on the exchange
    ///
    /// # Returns
    ///
    /// A vector of MarketSummaryStatic structs representing the markets
    ///
    /// # Errors
    ///
    /// If the markets cannot be retrieved
    pub async fn markets(&self) -> Result<Vec<MarketSummaryStatic>> {
        self.request(Method::Get, "/v1/markets".into(), None::<()>, None)
            .await
            .map(
                |result_container: ResultsContainer<Vec<MarketSummaryStatic>>| {
                    result_container.results
                },
            )
    }

    /// Check if the client has a private key set allowing for private API calls
    ///
    /// # Returns
    ///
    /// A boolean indicating if the client has a private key set
    pub(crate) fn is_private(&self) -> bool {
        self.l2_chain_private_key_account.is_some()
    }

    /// Get the current JWT token
    /// If the token is expired, it will be refreshed
    ///
    /// # Returns
    ///
    /// A string representing the current JWT token
    ///
    /// # Errors
    ///
    /// If the token cannot be refreshed
    pub async fn jwt(&self) -> Result<String> {
        // Check if Invalid
        if self.check_jwt_expired().await {
            self.refresh_jwt(false).await?;
        }

        // Return JWT
        let lock = self.jwt.read().await;
        let (_ts, jwt) = &*lock;
        Ok(jwt.clone())
    }

    /// Check if the current JWT token is expired
    ///
    /// # Returns
    ///
    /// A boolean indicating if the token is expired
    async fn check_jwt_expired(&self) -> bool {
        // Read Lock to check if JWT is valid
        let lock = self.jwt.read().await;
        let (ts, _jwt) = &*lock;
        SystemTime::now()
            .duration_since(*ts)
            .map_or(true, |duration| duration.as_secs() > JWT_UPDATE_INTERVAL)
    }

    /// Refresh the current JWT token
    /// Allows for a force update to bypass the check for expired token
    ///
    /// # Parameters
    ///
    /// * `force_update` - A boolean indicating if the token should be updated regardless of expiration
    ///
    /// # Errors
    ///
    /// If the token cannot be refreshed
    pub async fn refresh_jwt(&self, force_update: bool) -> Result<()> {
        // Write Lock to update JWT
        let mut lock = self.jwt.write().await;

        // Recheck if JWT is expired after acquiring write lock to prevent multiple updates at once with async calls
        let is_jwt_expired = {
            let (ts, _jwt) = &*lock;
            SystemTime::now()
                .duration_since(*ts)
                .map_or(true, |duration| duration.as_secs() > JWT_UPDATE_INTERVAL)
        };

        // Update JWT if expired or forced update is requested
        if is_jwt_expired || force_update {
            let (l2_chain, signing_key, account) = self
                .l2_chain_private_key_account
                .as_ref()
                .ok_or(Error::MissingPrivateKey)?;
            let (timestamp, headers) = auth_headers(l2_chain, signing_key, account)?;
            trace!("Auth Headers {headers:?}");
            let token = self
                .request::<&'static str, JWTToken>(
                    Method::Post,
                    "/v1/auth".into(),
                    Some(""),
                    Some(headers),
                )
                .await
                .map(|s| s.jwt_token)?;
            *lock = (timestamp, token);
        }
        Ok(())
    }

    /// Get the current BBO for a market
    ///
    /// # Parameters
    ///
    /// * `market_symbol` - A string representing the market symbol
    ///
    /// # Returns
    ///
    /// A BBO struct representing the best bid and offer for the market
    ///
    /// # Errors
    ///
    /// If the BBO cannot be retrieved
    pub async fn bbo(&self, market_symbol: String) -> Result<BBO> {
        self.request(
            Method::Get,
            format!("/v1/bbo/{market_symbol}"),
            None::<()>,
            None,
        )
        .await
    }

    /// Create an order on the exchange
    ///
    /// # Parameters
    ///
    /// * `order_request` - An OrderRequest struct representing the order to be created
    ///
    /// # Returns
    ///
    /// An OrderUpdate struct representing the order that was created
    ///
    /// # Errors
    ///
    /// If the order cannot be created
    pub async fn create_order(&self, order_request: OrderRequest) -> Result<OrderUpdate> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Error::TimeError(e.to_string()))?
            .as_millis();

        let (l2_chain, signing_key, account) = self
            .l2_chain_private_key_account
            .as_ref()
            .ok_or(Error::MissingPrivateKey)?;

        let order = sign_order(order_request, signing_key, timestamp, *l2_chain, *account)?;

        self.request_auth(Method::Post, "/v1/orders".into(), Some(order))
            .await
    }

    /// Cancel an order on the exchange by order ID
    ///
    /// # Parameters
    ///
    /// * `order_id` - A string representing the order ID to be cancelled
    ///
    /// # Returns
    ///
    /// A Result indicating success or failure
    ///
    /// # Errors
    ///
    /// If the order cannot be cancelled
    pub async fn cancel_order(&self, order_id: String) -> Result<()> {
        match self
            .request_auth::<(), ()>(Method::Delete, format!("/v1/orders/{order_id}"), None::<()>)
            .await
        {
            Ok(_) => Ok(()),
            Err(Error::RestEmptyResponse) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Cancel an order on the exchange by client ID
    ///
    /// # Parameters
    ///
    /// * `client_order_id` - A string representing the client order ID to be cancelled
    ///
    /// # Returns
    ///
    /// A Result indicating success or failure
    ///
    /// # Errors
    ///
    /// If the order cannot be cancelled
    pub async fn cancel_order_by_client_id(&self, client_order_id: String) -> Result<()> {
        match self
            .request_auth::<(), ()>(
                Method::Delete,
                format!("/v1/orders/by_client_id/{client_order_id}"),
                None::<()>,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(Error::RestEmptyResponse) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Cancel all orders on the exchange
    ///
    /// # Returns
    ///
    /// A vector of strings representing the order IDs that were cancelled
    ///
    /// # Errors
    ///
    /// If the orders cannot be cancelled
    pub async fn cancel_all_orders(&self) -> Result<Vec<String>> {
        self.request_auth(Method::Delete, "/v1/orders".into(), None::<()>)
            .await
    }

    /// Cancel all orders on the exchange for a specific market
    ///
    /// # Parameters
    ///
    /// * `market` - A string representing the market symbol to cancel orders for
    ///
    /// # Returns
    ///
    /// A vector of strings representing the order IDs that were cancelled
    ///
    /// # Errors
    ///
    /// If the orders cannot be cancelled
    pub async fn cancel_all_orders_for_market(&self, market: String) -> Result<Vec<String>> {
        self.request_auth(
            Method::Delete,
            format!("/v1/orders/?market={market}"),
            None::<()>,
        )
        .await
    }

    /// Get the Account Information
    ///
    /// # Returns
    ///
    /// An AccountInformation struct representing the account information
    ///
    /// # Errors
    ///
    /// If the account information cannot be retrieved
    pub async fn account_information(&self) -> Result<AccountInformation> {
        self.request_auth(Method::Get, "/v1/account".into(), None::<()>)
            .await
    }

    /// Get the balances for the account
    ///
    /// # Returns
    ///
    /// A Balances struct representing the account balances
    ///
    /// # Errors
    ///
    /// If the balances cannot be retrieved
    pub async fn balance(&self) -> Result<Balances> {
        self.request_auth(Method::Get, "/v1/balance".into(), None::<()>)
            .await
    }

    /// Get the positions for the account
    ///
    /// # Returns
    ///
    /// A Positions struct representing the account positions
    ///
    /// # Errors
    ///
    /// If the positions cannot be retrieved
    pub async fn positions(&self) -> Result<Positions> {
        self.request_auth(Method::Get, "/v1/positions".into(), None::<()>)
            .await
    }

    /// Perform a REST API request with authentication headers
    ///
    /// # Parameters
    ///
    /// * `method` - A Method enum representing the HTTP method to use
    /// * `path` - A string representing the path to the API endpoint
    /// * `body` - An optional serializable object representing the request body
    ///
    /// # Returns
    ///
    /// A Result with the deserialized response object
    ///
    /// # Errors
    ///
    /// If the request cannot be completed
    async fn request_auth<B: serde::Serialize, T: for<'de> serde::Deserialize<'de>>(
        &self,
        method: Method,
        path: String,
        body: Option<B>,
    ) -> Result<T> {
        let jwt = self.jwt().await?;
        let mut header_map: HeaderMap<HeaderValue> = HeaderMap::with_capacity(1);
        header_map.insert("Authorization", format!("Bearer {jwt}").parse().unwrap());
        self.request(method, path, body, Some(header_map)).await
    }

    /// Perform a REST API request with optional additional headers
    ///
    /// # Parameters
    ///
    /// * `method` - A Method enum representing the HTTP method to use
    /// * `path` - A string representing the path to the API endpoint
    /// * `body` - An optional serializable object representing the request body
    /// * `additional_headers` - An optional HeaderMap representing additional headers to include
    ///
    /// # Returns
    ///
    /// A Result with the deserialized response object
    ///
    /// # Errors
    ///
    /// If the request cannot be completed
    async fn request<B: serde::Serialize, T: for<'de> serde::Deserialize<'de>>(
        &self,
        method: Method,
        path: String,
        body: Option<B>,
        additional_headers: Option<HeaderMap<HeaderValue>>,
    ) -> Result<T> {
        let url = format!("{}{path}", self.url.rest());

        let mut request = match method {
            Method::Get => self.client.get(url),
            Method::Post => self.client.post(url),
            Method::Delete => self.client.delete(url),
        };

        if let Some(body_object) = body {
            request = request.json(&body_object);
        }

        request = request.header("Accept", "application/json");

        if let Some(headers) = additional_headers {
            request = request.headers(headers);
        }

        let result = request
            .send()
            .await
            .map_err(|e| Error::RestError(e.to_string()))?;
        let text = result
            .text()
            .await
            .map_err(|e| Error::RestError(e.to_string()))?;

        if text.is_empty() {
            return Err(Error::RestEmptyResponse);
        }

        serde_json::from_str(&text)
            .map_err(|e| Error::DeserializationError(format!("Text: {text} Error: {e:?}")))
    }
}
