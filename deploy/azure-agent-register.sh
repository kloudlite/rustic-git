#!/usr/bin/env bash
# Register the build VM as the self-hosted `build-0` agent for the Azure Pipeline.
#
# Run ON the VM, once:
#   AZP_TOKEN=<pat> bash ~/azure-agent-register.sh
#
# The PAT needs one scope: Agent Pools (read, manage). It is read from the environment and handed
# to config.sh, which exchanges it for the agent's own credential under ~/azagent/.credentials;
# this script never echoes or stores it. Re-running with a new PAT re-registers (`--replace`).
#
# Why self-hosted here when GitHub's runners are hosted: a new Azure DevOps org has no hosted
# parallelism until Microsoft grants it, and this repo is private, so no stranger's pull request
# can reach the agent — the objection to self-hosted on the public GitHub repo does not apply.
set -euo pipefail
: "${AZP_TOKEN:?set AZP_TOKEN to an Azure DevOps PAT with Agent Pools (read, manage)}"

AGENT_VERSION=4.258.1
mkdir -p ~/azagent && cd ~/azagent
if [ ! -f config.sh ]; then
  curl -sSL -o agent.tgz "https://download.agent.dev.azure.com/agent/${AGENT_VERSION}/vsts-agent-linux-x64-${AGENT_VERSION}.tar.gz"
  tar zxf agent.tgz && rm agent.tgz
fi

./config.sh --unattended \
  --url https://dev.azure.com/kloudlite \
  --auth pat --token "$AZP_TOKEN" \
  --pool build-0 --agent build-0 --work _work \
  --acceptTeeEula --replace

# A systemd service, so the agent survives reboots and does not depend on a shell staying open.
sudo ./svc.sh install "$USER"
sudo ./svc.sh start
sudo ./svc.sh status | tail -3
