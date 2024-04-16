use serde::Serialize;
use std::{env, fs, net::SocketAddr, panic, path::PathBuf, process, sync::Mutex};
use tauri_plugin_http::reqwest::{self, header::AUTHORIZATION};

use axum_jrpc::{JsonRpcAnswer, JsonRpcRequest, JsonRpcResponse};
use reqwest::header::CONTENT_TYPE;
use tari_common::initialize_logging;
use tari_dan_app_utilities::configuration::load_configuration;

use tari_crypto::{keys::PublicKey, ristretto::RistrettoPublicKey};
use tari_dan_wallet_daemon::{
    cli::Cli, config::ApplicationConfig, initialize_wallet_sdk, run_tari_dan_wallet_daemon,
};
use tari_dan_wallet_sdk::apis::key_manager;
use tari_shutdown::Shutdown;
use tari_wallet_daemon_client::{
    types::{
        AccountsCreateFreeTestCoinsRequest, AccountsCreateFreeTestCoinsResponse,
        AccountsGetBalancesRequest, AccountsGetBalancesResponse, AuthLoginAcceptRequest,
        AuthLoginAcceptResponse, AuthLoginRequest, AuthLoginResponse,
    },
    ComponentAddressOrName,
};
use tauri::{self, State};

struct Tokens {
    auth: Mutex<String>,
    permission: Mutex<String>,
}

#[tauri::command]
async fn wallet_daemon() -> String {
    tauri::async_runtime::spawn(async move {
        let _ = start_wallet_daemon().await;
    });
    format!("Wallet daemon started")
}

#[tauri::command]
async fn get_permission_token(tokens: State<'_, Tokens>) -> Result<(), ()> {
    let handle = tauri::async_runtime::spawn(async move { permission_token().await.unwrap() });
    let (permission_token, auth_token) = handle.await.unwrap();
    tokens
        .permission
        .lock()
        .unwrap()
        .replace_range(.., &permission_token);
    tokens.auth.lock().unwrap().replace_range(.., &auth_token);
    Ok(())
}

#[tauri::command]
async fn get_free_coins(tokens: State<'_, Tokens>) -> Result<(), ()> {
    let permission_token = tokens.permission.lock().unwrap().clone();
    let auth_token = tokens.auth.lock().unwrap().clone();
    let handle = tauri::async_runtime::spawn(async move {
        free_coins(auth_token.clone(), permission_token.clone())
            .await
            .unwrap()
    });
    handle.await.unwrap();
    Ok(())
}

#[tauri::command]
async fn get_balances(tokens: State<'_, Tokens>) -> Result<AccountsGetBalancesResponse, ()> {
    let permission_token = tokens.permission.lock().unwrap().clone();
    let auth_token = tokens.auth.lock().unwrap().clone();
    let handle = tauri::async_runtime::spawn(async move {
        balances(auth_token.clone(), permission_token.clone())
            .await
            .unwrap()
    });
    let balances = handle.await.unwrap();
    Ok(balances)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_shell::init())
        .manage(Tokens {
            permission: Mutex::new("".to_string()),
            auth: Mutex::new("".to_string()),
        })
        .invoke_handler(tauri::generate_handler![
            wallet_daemon,
            get_permission_token,
            get_free_coins,
            get_balances
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

async fn start_wallet_daemon() -> Result<(), anyhow::Error> {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        default_hook(info);
        process::exit(1);
    }));

    let base_path = env::current_dir().unwrap().parent().unwrap().to_path_buf();

    let cli = Cli::init();

    let config_path = PathBuf::from(base_path.join("config.toml"));
    let cfg = load_configuration(config_path, true, &cli).unwrap();
    let mut config = ApplicationConfig::load_from(&cfg).unwrap();
    let http_port = env::var("HTTP_PORT").unwrap().parse().unwrap();
    let http_ui_address = Some(SocketAddr::from(([0, 0, 0, 0], http_port)));
    config.dan_wallet_daemon.http_ui_address = http_ui_address;
    config.dan_wallet_daemon.indexer_node_json_rpc_url =
        env::var("INDEXER_NODE_JSON_RPC_URL").unwrap();
    let derive_secret = env::var("DERIVE_SECRET").ok();
    println!("base path: {:?}", config.common.base_path);

    if let Some(secret) = derive_secret {
        let index = secret.parse::<u64>().unwrap();
        let sdk = initialize_wallet_sdk(&config).unwrap();
        println!("1 Deriving secret key at index {}", index);
        let secret = sdk
            .key_manager_api()
            .derive_key(key_manager::TRANSACTION_BRANCH, index)
            .unwrap();
        println!("Secret: {}", secret.key.reveal());
        println!(
            "Public key: {}",
            RistrettoPublicKey::from_secret_key(&secret.key)
        );
    }
    // Remove the file if it was left behind by a previous run
    let _file = fs::remove_file(base_path.join("pid"));

    let shutdown = Shutdown::new();
    let shutdown_signal = shutdown.to_signal();

    if let Err(e) = initialize_logging(
        &base_path.join("log4rs.yml"),
        &base_path,
        include_str!("./log4rs_sample.yml"),
    ) {
        eprintln!("{}", e);
        return Err(e.into());
    }

    run_tari_dan_wallet_daemon(config, shutdown_signal).await
}

async fn permission_token() -> Result<(String, String), anyhow::Error> {
    let req_params = AuthLoginRequest {
        permissions: vec!["Admin".to_string()],
        duration: None,
    };
    let address = env::var("JSON_CONNECT_ADDRESS").unwrap().parse().unwrap();
    let req_res = make_request(address, None, "auth.request".to_string(), &req_params).await?;
    let req_res: AuthLoginResponse = serde_json::from_value(req_res)?;

    let auth_token = req_res.auth_token;
    println!("Auth token: {}", auth_token);

    let acc_params = AuthLoginAcceptRequest {
        auth_token: auth_token.clone(),
        name: auth_token.clone(),
    };
    let acc_res = make_request(address, None, "auth.accept".to_string(), &acc_params).await?;
    let acc_res: AuthLoginAcceptResponse = serde_json::from_value(acc_res)?;
    println!("Permissions token: {}", acc_res.permissions_token);

    Ok((acc_res.permissions_token, auth_token))
}

async fn free_coins(auth_token: String, permissions_token: String) -> Result<(), anyhow::Error> {
    let free_coins_params = AccountsCreateFreeTestCoinsRequest {
        account: Some(ComponentAddressOrName::Name(auth_token)),
        amount: 100_000_000.into(),
        max_fee: None,
        key_id: None,
    };
    let address = env::var("JSON_CONNECT_ADDRESS").unwrap().parse().unwrap();
    let free_coins_res = make_request(
        address,
        Some(permissions_token),
        "accounts.create_free_test_coins".to_string(),
        free_coins_params,
    )
    .await?;
    let free_coins_res: AccountsCreateFreeTestCoinsResponse =
        serde_json::from_value(free_coins_res)?;

    println!("free_coins_res: {:?}", &free_coins_res);

    Ok(())
}

async fn balances(
    auth_token: String,
    permissions_token: String,
) -> Result<AccountsGetBalancesResponse, anyhow::Error> {
    let balance_req = AccountsGetBalancesRequest {
        account: Some(ComponentAddressOrName::Name(auth_token)),
        refresh: false,
    };
    let address = env::var("JSON_CONNECT_ADDRESS").unwrap().parse().unwrap();
    let balance_res = make_request(
        address,
        Some(permissions_token),
        "accounts.get_balances".to_string(),
        balance_req,
    )
    .await?;
    let balance_res: AccountsGetBalancesResponse = serde_json::from_value(balance_res)?;

    println!("balance_res: {:?}", &balance_res);

    Ok(balance_res)
}

async fn make_request<T: Serialize>(
    address: SocketAddr,
    token: Option<String>,
    method: String,
    params: T,
) -> Result<serde_json::Value, anyhow::Error> {
    let url = format!("http://{}", address);
    let client = reqwest::Client::new();
    let body = JsonRpcRequest {
        id: 0,
        jsonrpc: "2.0".to_string(),
        method,
        params: serde_json::to_value(params)?,
    };
    let mut builder = client.post(url).header(CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    let resp = builder
        .json(&body)
        .send()
        .await?
        .json::<JsonRpcResponse>()
        .await?;
    match resp.result {
        JsonRpcAnswer::Result(result) => Ok(result),
        JsonRpcAnswer::Error(error) => Err(anyhow::Error::msg(error.to_string())),
    }
}