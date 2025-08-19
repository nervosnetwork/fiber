# E2E test for funding tx verification

Start nodes via the following commands:

```
# Set a logging file for the shell builder for debugging
export FUNDING_TX_SHELL_BUILDER_LOG_FILE=/tmp/funding.log
# Choose test case via the environment variable
export EXTRA_BRU_ARGS="--env-var FUNDING_TX_VERIFICATION_CASE=fund_from_peer"
# Start with a clean environment
export REMOVE_OLD_STATE=y

./tests/nodes/start.sh e2e/funding-tx-verification
```
