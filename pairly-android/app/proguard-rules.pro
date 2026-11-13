# Project-specific R8 rules. Core binding rules come from :core-bindings consumer-rules.pro.

# The screen helper is started by name with app_process (outside the app), so keep it whole.
-keep class dev.pairly.android.helper.** { *; }
