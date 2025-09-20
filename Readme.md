# Polygon Arbitrage Opportunity Detector — Day-3

**What this repo contains**
- A Rust bot that connects to Polygon RPC, queries two DEX routers (QuickSwap & SushiSwap),
  computes simulated arbitrage (WETH → USDC), estimates gas (with fallback), applies slippage,
  and logs profitable opportunities into a local SQLite DB.

**Important:** This project **detects & logs** opportunities only. It does **not** execute on-chain trades.

## Quick demo (run locally)
1. Copy `.env.example` → `.env` and fill `POLYGON_RPC` with your Alchemy / RPC endpoint:
   ```powershell
   copy .env.example .env
   # edit .env to add your key
