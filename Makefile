build:
	cargo build --release

pluginify:
	spin pluginify

install: build pluginify
	spin plugins install --yes --file ./trigger-tailscale.json
