# How to Run WASM Test Runner

## Ubuntu

> It's assumed that all code blocks are running in the repository top directory.

1\. Install rust, nodejs and pnpm.

2\. Install dependencies:

```
cargo install wasm-pack
# or cargo binstall wasm-pack

cd .github/wasm-test-runner
pnpm i
pnpm dlx puppeteer browsers install chrome
sudo apt install -y xvfb ca-certificates fonts-liberation libasound2t64 libatk-bridge2.0-0 \
  libatk1.0-0 libc6 libcairo2 libcups2 libdbus-1-3 libexpat1 libfontconfig1 libgbm1 libgcc1 \
  libglib2.0-0 libgtk-3-0 libnspr4 libnss3 libpango-1.0-0 libpangocairo-1.0-0 libstdc++6 \
  libx11-6 libx11-xcb1 libxcb1 libxcomposite1 libxcursor1 libxdamage1 libxext6 libxfixes3 \
  libxi6 libxrandr2 libxrender1 libxss1 libxtst6 lsb-release wget xdg-utils
```

3\. Run the test

```
export WORKING_DIR="$(pwd)"
cd .github/wasm-test-runner
pnpm exec node index.js
```