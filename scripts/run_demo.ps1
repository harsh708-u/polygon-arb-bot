# run_demo.ps1
# Demo runner script for Polygon Arbitrage Bot
# Author: Harshvardhan Rathore

Write-Host " Starting Polygon Arbitrage Bot..."

# Step 1: Check if .env file exists
if (!(Test-Path ".env")) {
    Write-Host "⚠️  .env file not found! Please create it by copying .env.example and adding your API key + private key."
    exit 1
}

# Step 2: Build the bot in release mode
Write-Host "📦 Building project..."
cargo build --release
if ($LASTEXITCODE -ne 0) {
    Write-Host "❌ Build failed! Please check your Rust installation or Cargo.toml."
    exit 1
}

# Step 3: Run the bot
Write-Host "▶️ Running bot..."
cargo run --release

Write-Host "✅ Bot stopped. Check opportunities.sqlite3 or opportunities.csv for logged trades."
