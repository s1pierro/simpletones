#!/data/data/com.termux/files/usr/bin/bash
# build.sh — compile, empaquette, aligne et signe l'APK, directement dans Termux.
#
# Usage :  ./build.sh            → construit dist/<app>.apk
#          ./build.sh install    → construit puis ouvre l'installeur Android
set -euo pipefail
cd "$(dirname "$0")"

# Nom de la crate lu dans Cargo.toml (= android.app.lib_name du manifeste).
APP="$(sed -n 's/^name *= *"\(.*\)"/\1/p' Cargo.toml | head -n1)"
OUT=dist
KEYSTORE="$HOME/.android/debug.keystore"

say()  { printf '\033[1;36m▸ %s\033[0m\n' "$*"; }
fail() { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

# --- 0. Vérifications --------------------------------------------------------
case "${PREFIX:-}" in
  *com.termux*) ;;
  *) fail "Lance ce script dans Termux (pas dans Debian) : le Rust de Termux compile nativement pour Android." ;;
esac

missing=()
need() { command -v "$1" >/dev/null 2>&1 || missing+=("$1 (paquet probable : $2)"); }
need cargo     rust
need zipalign  "zipalign ou aapt"
need apksigner apksigner
need keytool   openjdk-17
need zip       zip

# aapt2 de préférence ; sinon l'ancien aapt (fourni par le paquet aapt) fait le même travail ici.
if command -v aapt2 >/dev/null 2>&1; then PACKER=aapt2
elif command -v aapt >/dev/null 2>&1; then PACKER=aapt
else missing+=("aapt2 ou aapt (paquet probable : aapt)")
fi

if ((${#missing[@]})); then
  printf '\033[1;31m✗ Commandes introuvables :\033[0m\n' >&2
  printf '   - %s\n' "${missing[@]}" >&2
  printf '   Pour savoir quel paquet fournit une commande : pkg search <nom>\n' >&2
  exit 1
fi

# android.jar : la définition de l'API Android, requise par aapt2.
ANDROID_JAR="${ANDROID_JAR:-}"
# Cherché aussi dans les projets voisins (ex. ../hello-apk) pour ne pas retélécharger.
for c in "$ANDROID_JAR" ./android.jar ../*/android.jar "$PREFIX/share/aapt/android.jar" "$PREFIX/share/java/android.jar"; do
  [ -n "$c" ] && [ -f "$c" ] && { ANDROID_JAR="$c"; break; }
done
if [ ! -f "${ANDROID_JAR:-}" ]; then
  # Téléchargement automatique depuis le dépôt officiel du SDK Android (une seule fois).
  PLATFORM_ZIP=platform-30_r03.zip
  say "android.jar absent : téléchargement de $PLATFORM_ZIP (≈ 50 Mo, une seule fois)…"
  command -v curl  >/dev/null || fail "curl absent : pkg install curl"
  command -v unzip >/dev/null || fail "unzip absent : pkg install unzip"
  tmpd="$(mktemp -d)"
  curl -fL --progress-bar -o "$tmpd/$PLATFORM_ZIP" "https://dl.google.com/android/repository/$PLATFORM_ZIP" \
    || { rm -rf "$tmpd"; fail "Téléchargement impossible. Récupère android.jar à la main et place-le ici (./android.jar)."; }
  unzip -q -j -o "$tmpd/$PLATFORM_ZIP" '*/android.jar' -d . \
    || { rm -rf "$tmpd"; fail "android.jar introuvable dans l'archive téléchargée."; }
  rm -rf "$tmpd"
  ANDROID_JAR=./android.jar
fi

case "$(uname -m)" in
  aarch64)        ABI=arm64-v8a ;;
  armv7l|armv8l)  ABI=armeabi-v7a ;;
  x86_64)         ABI=x86_64 ;;
  *) fail "Architecture non gérée : $(uname -m)" ;;
esac

# --- 1. Compilation Rust (native, aucune cross-compilation) -----------------
say "Compilation Rust ($ABI)…"
cargo build --release
SO="target/release/lib$APP.so"
[ -f "$SO" ] || fail "$SO n'a pas été produit."

# --- 2. APK de base (manifeste) ---------------------------------------------
say "Empaquetage ($PACKER, $ANDROID_JAR)…"
rm -rf "$OUT/tmp" && mkdir -p "$OUT/tmp/lib/$ABI"
if [ "$PACKER" = aapt2 ]; then
  aapt2 compile --dir res -o "$OUT/tmp/res.zip"
  aapt2 link -o "$OUT/tmp/base.apk" -I "$ANDROID_JAR" --manifest AndroidManifest.xml "$OUT/tmp/res.zip"
else
  aapt package -f -M AndroidManifest.xml -S res -I "$ANDROID_JAR" -F "$OUT/tmp/base.apk"
fi

# --- 3. Ajout de la bibliothèque native -------------------------------------
cp "$SO" "$OUT/tmp/lib/$ABI/"
( cd "$OUT/tmp" && zip -q -r base.apk lib )

# --- 4. Alignement ----------------------------------------------------------
say "Alignement…"
zipalign -p -f 4 "$OUT/tmp/base.apk" "$OUT/tmp/aligned.apk"

# --- 5. Signature (clé de debug créée une seule fois) -----------------------
if [ ! -f "$KEYSTORE" ]; then
  say "Création de la clé de debug ($KEYSTORE)…"
  mkdir -p "$(dirname "$KEYSTORE")"
  keytool -genkeypair -keystore "$KEYSTORE" -alias androiddebugkey \
    -storepass android -keypass android -keyalg RSA -keysize 2048 \
    -validity 10000 -dname "CN=Android Debug,O=Android,C=US" >/dev/null 2>&1
fi
say "Signature…"
apksigner sign --ks "$KEYSTORE" --ks-key-alias androiddebugkey \
  --ks-pass pass:android --key-pass pass:android \
  --out "$OUT/$APP.apk" "$OUT/tmp/aligned.apk"
rm -rf "$OUT/tmp" "$OUT/$APP.apk.idsig"

say "APK prêt : $OUT/$APP.apk ($(du -h "$OUT/$APP.apk" | cut -f1))"

# --- 6. Installation (optionnelle) ------------------------------------------
if [ "${1:-}" = "install" ]; then
  command -v termux-open >/dev/null || fail "termux-open absent : pkg install termux-tools"
  say "Ouverture de l'installeur Android…"
  termux-open "$OUT/$APP.apk"
fi
