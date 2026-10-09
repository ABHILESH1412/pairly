"""Put the update key's public half (keys/update.pub, minisign format) into tauri.conf.json,
base64-encoded as Tauri's updater expects. Run from the repository root."""
import base64
import json

PUB = "keys/update.pub"
CONF = "pairly-windows/src-tauri/tauri.conf.json"

key = open(PUB, encoding="utf-8").read()
if "PLACEHOLDER" in key:
    print(f"warning: {PUB} is still the placeholder; updates won't verify")
conf = json.load(open(CONF, encoding="utf-8"))
conf["plugins"]["updater"]["pubkey"] = base64.b64encode(key.encode()).decode()
with open(CONF, "w", encoding="utf-8") as f:
    json.dump(conf, f, indent=2)
    f.write("\n")
