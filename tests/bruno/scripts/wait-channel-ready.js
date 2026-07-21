const axios = require("axios");

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function rpc(url, method, params) {
  const response = await axios.post(url, {
    id: "42",
    jsonrpc: "2.0",
    method,
    params,
  });

  if (response.data.error) {
    throw new Error(JSON.stringify(response.data.error));
  }

  return response.data.result;
}

async function waitChannelReady({
  bru,
  rpcUrl,
  rpcUrls,
  peerPubkey,
  channelIdVar = "CHANNEL_ID",
  maxAttempts = 90,
  intervalMs = 1000,
}) {
  const channelId = bru.getVar(channelIdVar);
  const urls = (rpcUrls || [rpcUrl]).map((url) => bru.getEnvVar(url) || url);
  const lastStates = new Map();

  for (let attempt = 0; attempt < maxAttempts; attempt++) {
    let allReady = true;

    for (const url of urls) {
      const params = rpcUrls ? [{}] : [{ pubkey: peerPubkey }];
      const result = await rpc(url, "list_channels", params);
      const channels = result.channels || [];
      const channel = channels.find((item) => item.channel_id === channelId);
      const lastState = channel && channel.state && channel.state.state_name;
      lastStates.set(url, lastState);

      if (lastState !== "ChannelReady") {
        allReady = false;
      }
    }

    if (allReady) {
      return;
    }

    await sleep(intervalMs);
  }

  throw new Error(
    `channel did not reach ChannelReady, channel_id=${channelId}, attempts=${maxAttempts}, last_states=${JSON.stringify(Object.fromEntries(lastStates))}`,
  );
}

module.exports = { waitChannelReady };
