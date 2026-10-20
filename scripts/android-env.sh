# Source this to get the Android + Flutter toolchain on PATH (user-space install, no sudo):
#   source scripts/android-env.sh
export JAVA_HOME=/usr/lib/jvm/java-17-openjdk
export ANDROID_HOME="$HOME/Android" ANDROID_SDK_ROOT="$HOME/Android"
export PATH="$HOME/.local/flutter/bin:$HOME/Android/cmdline-tools/latest/bin:$HOME/Android/platform-tools:$PATH"
