# Source this file to keep all toolchain state inside the project:
#   . scripts/env.sh
UD_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
export UD_ROOT
export UD_TOOLCHAIN="$UD_ROOT/.toolchain"

export ANDROID_HOME="$UD_TOOLCHAIN/android-sdk"
export ANDROID_USER_HOME="$UD_TOOLCHAIN/android-user"
export GRADLE_USER_HOME="$UD_TOOLCHAIN/gradle"
export CARGO_HOME="$UD_TOOLCHAIN/cargo"

if [ -d /usr/lib/jvm/java-21-openjdk ]; then
    export JAVA_HOME=/usr/lib/jvm/java-21-openjdk
fi

case ":$PATH:" in
    *":$ANDROID_HOME/cmdline-tools/latest/bin:"*) ;;
    *) export PATH="$ANDROID_HOME/cmdline-tools/latest/bin:$PATH" ;;
esac
