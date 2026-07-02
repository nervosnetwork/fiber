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
  peerPubkey,
  channelIdVar = "CHANNEL_ID",
  maxAttempts = 20,
  intervalMs = 1000,
}) {
  const channelId = bru.getVar(channelIdVar);
  let lastState;

  for (let attempt = 0; attempt < maxAttempts; attempt++) {
    const result = await rpc(rpcUrl, "list_channels", [{ pubkey: peerPubkey }]);
    const channels = result.channels || [];
    const channel = channels.find((item) => item.channel_id === channelId);
    lastState = channel && channel.state && channel.state.state_name;

    if (lastState === "ChannelReady") {
      return;
    }

    await sleep(intervalMs);
  }

  throw new Error(`channel did not reach ChannelReady, channel_id=${channelId}, last_state=${String(lastState)}`);
}

module.exports = { waitChannelReady };
