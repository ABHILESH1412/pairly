"""latest.json for Tauri's updater: the newest version, where its installer is, and the
installer's signature. Usage: latest_json.py VERSION TAG INSTALLER_NAME SIGNATURE_FILE"""
import datetime
import json
import sys

version, tag, name, sig_file = sys.argv[1:5]
print(json.dumps({
    "version": version,
    "notes": f"Pairly {version}",
    "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "platforms": {
        "windows-x86_64": {
            "signature": open(sig_file, encoding="utf-8").read().strip(),
            "url": f"https://github.com/ABHILESH1412/pairly/releases/download/{tag}/{name}",
        },
    },
}, indent=2))
