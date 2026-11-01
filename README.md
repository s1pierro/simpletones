# Simpletones

Petit synthétiseur pour Android, écrit en Rust sans aucune dépendance. L'APK
fait moins de 200 Ko.

- Clavier tactile de deux octaves, de Do4 à Do6
- Curseurs d'attaque et de relâche
- Son via AAudio, chargé à l'exécution avec `dlopen`
- Interface dessinée en logiciel dans une `NativeActivity`

Nécessite Android 8.0 (API 26) ou plus récent.

## Construire l'APK

Le build se fait directement sur Android, dans [Termux](https://termux.dev),
avec le Rust natif de Termux : pas de SDK ni de NDK, pas de cross-compilation.

    pkg install rust aapt apksigner zip unzip curl openjdk-17
    ./build.sh

L'APK est produit dans `dist/audiotest.apk`. `./build.sh install` le construit
puis ouvre l'installeur Android.

Au premier lancement, `build.sh` télécharge `android.jar` depuis le dépôt du SDK
Android (environ 50 Mo) et crée une clé de debug dans `~/.android/`.
