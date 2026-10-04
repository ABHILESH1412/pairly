# JNA and the UniFFI-generated bindings are accessed reflectively.
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-keep class * extends com.sun.jna.** { *; }
-keep class uniffi.** { *; }
# The generated bindings (RustBuffer and the callback interfaces are reached through JNA).
-keep class dev.pairly.core.ffi.** { *; }
-dontwarn java.awt.**
