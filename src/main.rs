use ethers::prelude::*;
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::types::U256;
use eyre::Result;
use std::convert::TryFrom;
use std::env;
use std::sync::Arc;
use chrono::Utc;
use rusqlite::{params, Connection};
use dotenv::dotenv;
use futures::future::try_join;

abigen!(
    UniswapV2Router,
    "./abi/IUniswapV2Router02.json"
);

// ---------- helpers ----------
fn format_amount(amount: U256, decimals: usize) -> String {
    let s = amount.to_string();
    if decimals == 0 { return s; }
    if s.len() <= decimals {
        let zeros = "0".repeat(decimals - s.len());
        let frac = format!("{}{}", zeros, s);
        let frac_trimmed = frac.trim_end_matches('0');
        if frac_trimmed.is_empty() { "0".to_string() } else { format!("0.{}", frac_trimmed) }
    } else {
        let int_part = &s[..s.len() - decimals];
        let frac_part = &s[s.len() - decimals..];
        let frac_trimmed = frac_part.trim_end_matches('0');
        if frac_trimmed.is_empty() { int_part.to_string() } else { format!("{}.{}", int_part, frac_trimmed) }
    }
}

fn u256_to_f64(amount: U256, decimals: usize) -> f64 {
    let s = amount.to_string();
    if decimals == 0 {
        return s.parse::<f64>().unwrap_or(0.0);
    }
    if s.len() <= decimals {
        let zeros = "0".repeat(decimals - s.len());
        let combined = format!("0.{}{}", zeros, s);
        combined.parse::<f64>().unwrap_or(0.0)
    } else {
        let int_part = &s[..s.len() - decimals];
        let frac_part = &s[s.len() - decimals..];
        let combined = format!("{}.{}", int_part, frac_part);
        combined.parse::<f64>().unwrap_or(0.0)
    }
}

fn build_token_u256(amount: f64, decimals: usize) -> U256 {
    let whole = amount.trunc() as u128;
    let frac = amount.fract();
    let whole_part = U256::from(whole) * U256::exp10(decimals);
    let frac_part = U256::from((frac * 10f64.powi(decimals as i32)).round() as u128);
    whole_part + frac_part
}

// ---------- sqlite helpers ----------
fn ensure_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS opportunities (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL,
            dex_buy TEXT NOT NULL,
            price_buy REAL NOT NULL,
            dex_sell TEXT NOT NULL,
            price_sell REAL NOT NULL,
            trade_size_weth REAL NOT NULL,
            gross_profit_usdc REAL NOT NULL,
            gas_cost_usdc REAL NOT NULL,
            net_profit_usdc REAL NOT NULL,
            tx_estimated_gas INTEGER,
            notes TEXT
        );",
    )?;
    Ok(())
}

fn insert_opportunity(
    conn: &Connection,
    ts: &str,
    dex_buy: &str,
    price_buy: f64,
    dex_sell: &str,
    price_sell: f64,
    trade_size_weth: f64,
    gross: f64,
    gas_usdc: f64,
    net: f64,
    tx_gas: Option<u64>,
    notes: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO opportunities (ts, dex_buy, price_buy, dex_sell, price_sell, trade_size_weth, gross_profit_usdc, gas_cost_usdc, net_profit_usdc, tx_estimated_gas, notes)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![ts, dex_buy, price_buy, dex_sell, price_sell, trade_size_weth, gross, gas_usdc, net, tx_gas, notes],
    )?;
    Ok(())
}

// ---------- fetch with timeout ----------
async fn fetch_amounts_with_timeout(
    router: &UniswapV2Router<Provider<Http>>,
    amount_in: U256,
    path: Vec<Address>,
    timeout_ms: u64,
) -> eyre::Result<Vec<U256>> {
    use tokio::time::{timeout, Duration};
    // build call first to avoid temporaries being dropped early
    let call = router.get_amounts_out(amount_in, path);
    let fut = call.call();
    match timeout(Duration::from_millis(timeout_ms), fut).await {
        Ok(Ok(amounts)) => Ok(amounts),
        Ok(Err(e)) => Err(eyre::eyre!("Contract call error: {}", e)),
        Err(_) => Err(eyre::eyre!("Request timed out after {}ms", timeout_ms)),
    }
}

// ---------- mock fetch (TEST_MODE) ----------
async fn mock_fetch_amounts(base: f64, delta: f64) -> eyre::Result<Vec<U256>> {
    let input_raw = U256::exp10(18);
    let output_value = (base + delta).max(0.0);
    let out_raw_u128 = (output_value * 1e6).round() as u128;
    let output_raw = U256::from(out_raw_u128);
    Ok(vec![input_raw, output_raw])
}

// ---------- estimate gas and convert to USDC ----------
async fn estimate_gas_cost_usdc(
    provider: Arc<Provider<Http>>,
    router: &UniswapV2Router<Provider<Http>>,
    router_addr: Address,
    path: Vec<Address>,
    amount_in: U256,
    wmatic: Address,
    usdc: Address,
    from_addr: Address,
) -> eyre::Result<(f64, Option<u64>)> {
    // Build calldata for swapExactTokensForTokens(amountIn, 0, path, to, deadline)
    let amount_out_min = U256::zero();
    let to = from_addr;
    let deadline = U256::from((chrono::Utc::now().timestamp() + 300) as u64);

    // build call, then calldata
    let call = router.swap_exact_tokens_for_tokens(amount_in, amount_out_min, path.clone(), to, deadline);
    let calldata = call.calldata().ok_or_else(|| eyre::eyre!("Failed to build calldata"))?;

    let tx = TransactionRequest::new()
        .to(NameOrAddress::Address(router_addr))
        .from(from_addr)
        .data(calldata);

    // convert to TypedTransaction
    let tx_typed: TypedTransaction = tx.clone().into();

    // estimate gas
    let gas_estimate = provider.estimate_gas(&tx_typed, None).await
        .map_err(|e| eyre::eyre!("estimate_gas failed: {}", e))?;

    // gas price and wei cost
    let gas_price = provider.get_gas_price().await?;
    let gas_cost_wei = gas_estimate.checked_mul(gas_price).unwrap_or(U256::zero());
    let gas_cost_matic = u256_to_f64(gas_cost_wei, 18);

    if gas_cost_matic <= 0.0 {
        return Ok((0.0, Some(gas_estimate.as_u64())));
    }

    // convert WMATIC -> USDC via router getAmountsOut
    let gas_matic_raw = build_token_u256(gas_cost_matic, 18);
    let path_wmatic_usdc = vec![wmatic, usdc];
    let amounts = router.get_amounts_out(gas_matic_raw, path_wmatic_usdc).call().await?;
    let gas_usdc = u256_to_f64(amounts[1], 6);

    Ok((gas_usdc, Some(gas_estimate.as_u64())))
}

// ---------- detection once ----------
async fn detect_once(
    provider: Arc<Provider<Http>>,
    router1: &UniswapV2Router<Provider<Http>>,
    router2: &UniswapV2Router<Provider<Http>>,
    dex1_addr: Address,
    dex2_addr: Address,
    weth: Address,
    usdc: Address,
    wmatic: Address,
    trade_size_weth: f64,
    slippage_tol: f64,
    min_profit_usdc: f64,
    fallback_gas_usdc: f64,
    estimate_from: Address,
    db_conn: &Connection,
    test_mode: bool,
    test_base: f64,
    test_spread: f64,
) -> eyre::Result<()> {
    let amount_in = build_token_u256(trade_size_weth, 18);
    let path = vec![weth, usdc];

    // fetch both concurrently or use mock
    let (r1, r2) = if test_mode {
        let half = test_spread / 2.0;
        (mock_fetch_amounts(test_base, half).await?, mock_fetch_amounts(test_base, -half).await?)
    } else {
        let timeout_ms = 2000u64;
        let f1 = fetch_amounts_with_timeout(router1, amount_in, path.clone(), timeout_ms);
        let f2 = fetch_amounts_with_timeout(router2, amount_in, path.clone(), timeout_ms);
        try_join(f1, f2).await.map_err(|e| eyre::eyre!("dual fetch failed: {}", e))?
    };

    let out1 = u256_to_f64(r1[1], 6);
    let out2 = u256_to_f64(r2[1], 6);
    println!("DEX1 -> {:.6} USDC, DEX2 -> {:.6} USDC", out1, out2);

    // decide buy/sell
    let (buy_name, sell_name, buy_out, sell_out, buy_router_addr, buy_router_ref) =
        if out1 < out2 {
            ("DEX1", "DEX2", out1, out2, dex1_addr, router1)
        } else {
            ("DEX2", "DEX1", out2, out1, dex2_addr, router2)
        };

    let gross = sell_out - buy_out;

    // attempt gas estimate
    let gas_result = estimate_gas_cost_usdc(
        provider.clone(),
        buy_router_ref,
        buy_router_addr,
        vec![weth, usdc],
        amount_in,
        wmatic,
        usdc,
        estimate_from,
    ).await;

    let (gas_usdc, gas_est_opt) = match gas_result {
        Ok((g_usdc, g_est)) => (g_usdc, g_est),
        Err(err) => {
            eprintln!("Gas estimate failed (falling back): {}", err);
            (fallback_gas_usdc, None)
        }
    };

    // slippage and net
    let sell_after_slippage = sell_out * (1.0 - slippage_tol);
    let gross_after_slippage = sell_after_slippage - buy_out;
    let net = gross_after_slippage - gas_usdc;

    println!("Buy on {} at {:.6}, Sell on {} at {:.6}", buy_name, buy_out, sell_name, sell_out);
    println!("Gross: {:.6}, Gross(after slippage): {:.6}, Gas(USDC): {:.6}, Net: {:.6}",
        gross, gross_after_slippage, gas_usdc, net);

    // dedupe & cooldown check before logging
    let mut should_log = true;
    let eps = 0.1_f64;            // require at least 0.1 USDC improvement
    let cooldown_secs = 60_i64;   // cooldown window in seconds

    let mut stmt = db_conn.prepare("SELECT ts, net_profit_usdc FROM opportunities ORDER BY id DESC LIMIT 1")?;
    let mut rows = stmt.query([])?;
    if let Some(row) = rows.next()? {
        let last_ts: String = row.get(0)?;
        let last_net: f64 = row.get(1)?;
        if let Ok(last_dt) = chrono::DateTime::parse_from_rfc3339(&last_ts) {
            let secs = (Utc::now() - last_dt.with_timezone(&Utc)).num_seconds();
            if !(net > last_net + eps || secs > cooldown_secs) {
                should_log = false;
            }
        }
    }

    if net >= min_profit_usdc && should_log {
        let ts = Utc::now().to_rfc3339();
        let notes = format!("gas_est={:?}", gas_est_opt);
        insert_opportunity(
            db_conn,
            &ts,
            buy_name,
            buy_out,
            sell_name,
            sell_out,
            trade_size_weth,
            gross,
            gas_usdc,
            net,
            gas_est_opt,
            Some(&notes),
        )?;
        println!("Arbitrage opportunity logged (net {:.6} USDC)!", net);
    } else if net >= min_profit_usdc {
        println!("Opportunity detected but skipped logging due to cooldown/duplicate.");
    } else {
        println!("No profitable opportunity >= {:.2} USDC", min_profit_usdc);
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    // config from env
    let rpc_url = env::var("POLYGON_RPC")?;
    let dex1_router: Address = env::var("DEX1_ROUTER")?.parse()?;
    let dex2_router: Address = env::var("DEX2_ROUTER")?.parse()?;
    let weth: Address = env::var("WETH_ADDRESS")?.parse()?;
    let usdc: Address = env::var("USDC_ADDRESS")?.parse()?;
    let wmatic: Address = env::var("WMATIC_ADDRESS")?.parse()?;

    let trade_size_weth: f64 = env::var("TRADE_SIZE_WETH").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let slippage_tol: f64 = env::var("SLIPPAGE_TOLERANCE").ok().and_then(|v| v.parse().ok()).unwrap_or(0.005);
    let min_profit_usdc: f64 = env::var("MIN_PROFIT_USDC").ok().and_then(|v| v.parse().ok()).unwrap_or(5.0);
    let poll_interval_seconds: u64 = env::var("POLL_INTERVAL_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let fallback_gas_usdc: f64 = env::var("FALLBACK_GAS_USDC").ok().and_then(|v| v.parse().ok()).unwrap_or(12.0);
    let estimate_from: Address = env::var("ESTIMATE_FROM_ADDRESS").ok().and_then(|v| v.parse().ok()).unwrap_or(Address::zero());
    let db_file = env::var("DB_FILE").unwrap_or_else(|_| "opportunities.sqlite3".to_string());

    // test controls
    let max_iters: Option<u64> = env::var("MAX_ITERS").ok().and_then(|v| v.parse().ok()).filter(|&n| n > 0);
    let test_mode: bool = env::var("TEST_MODE").ok().and_then(|v| v.parse().ok()).unwrap_or(false);
    let test_base: f64 = env::var("TEST_BASE_PRICE").ok().and_then(|v| v.parse().ok()).unwrap_or(4400.0);
    let test_spread: f64 = env::var("TEST_SPREAD_USDC").ok().and_then(|v| v.parse().ok()).unwrap_or(50.0);

    // provider & routers
    let provider_plain = Provider::<Http>::try_from(rpc_url.as_str())?.interval(std::time::Duration::from_millis(300));
    let provider = Arc::new(provider_plain);
    let router1 = UniswapV2Router::new(dex1_router, provider.clone());
    let router2 = UniswapV2Router::new(dex2_router, provider.clone());

    // quick sanity check
    let chain_id = provider.get_chainid().await?;
    println!("Connected. Chain ID: {}", chain_id);

    // ensure DB
    let conn = Connection::open(&db_file)?;
    ensure_db(&conn)?;

    // scheduler loop with graceful shutdown and MAX_ITERS
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_interval_seconds));
    let mut iter_count: u64 = 0;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                iter_count += 1;
                if let Err(e) = detect_once(
                    provider.clone(),
                    &router1,
                    &router2,
                    dex1_router,
                    dex2_router,
                    weth,
                    usdc,
                    wmatic,
                    trade_size_weth,
                    slippage_tol,
                    min_profit_usdc,
                    fallback_gas_usdc,
                    estimate_from,
                    &conn,
                    test_mode,
                    test_base,
                    test_spread,
                ).await {
                    eprintln!("Detection iteration error: {}", e);
                }

                if let Some(max) = max_iters {
                    if iter_count >= max {
                        println!("Reached MAX_ITERS ({}). Exiting.", max);
                        break;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("Ctrl-C received, shutting down gracefully.");
                break;
            }
        }
    }

    println!("Bot stopped.");
    Ok(())
}
