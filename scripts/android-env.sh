# Source this to add optional Android + Flutter tool locations to PATH.
#   source scripts/android-env.sh
if [ -z "${JAVA_HOME:-}" ] && [ -d /usr/lib/jvm/java-17-openjdk ]; then
  export JAVA_HOME=/usr/lib/jvm/java-17-openjdk
fi
if [ -z "${ANDROID_HOME:-}" ] && [ -d "$HOME/Android" ]; then
  export ANDROID_HOME="$HOME/Android"
fi
if [ -z "${ANDROID_SDK_ROOT:-}" ] && [ -n "${ANDROID_HOME:-}" ]; then
  export ANDROID_SDK_ROOT="$ANDROID_HOME"
fi
for tool_dir in "${FLUTTER_ROOT:-$HOME/.local/flutter}/bin" "${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Android}}/cmdline-tools/latest/bin" "${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$HOME/Android}}/platform-tools"; do
  if [ -d "$tool_dir" ]; then
    case ":$PATH:" in *":$tool_dir:"*) ;; *) export PATH="$tool_dir:$PATH" ;; esac
  fi
done
