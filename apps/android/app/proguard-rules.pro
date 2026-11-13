# The Rust core is reached through JNA, which finds these classes and their
# members by name at run time, so they must keep their names.
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-keep class uniffi.** { *; }
-dontwarn java.awt.**

# The QR scanner's parts are created by name at run time.
-keep class * implements com.google.firebase.components.ComponentRegistrar { <init>(); }
-keep class com.google.mlkit.** { <init>(); }
