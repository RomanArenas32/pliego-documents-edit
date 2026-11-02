# Clave de firma de Android

Android solo instala una actualización si viene firmada con la **misma clave** que la versión
anterior. Si la clave se pierde, quienes ya tienen Pliego instalado tendrán que desinstalarlo
para recibir la versión nueva. Guárdala con cuidado.

## Dónde está

- La clave: `~/Documents/Pliego firma Android/pliego-release.jks`
- Su contraseña: `~/Documents/Pliego firma Android/contraseña.txt`

Ninguna de las dos se sube al repositorio. Haz una copia de las dos (un pendrive, tu gestor de
contraseñas o un disco en la nube privado) y no las compartas.

## Compilar firmado en esta computadora

`src-tauri/gen/android/keystore.properties` (excluido de git) indica dónde está la clave:

```
storeFile=/Users/<usuario>/Documents/Pliego firma Android/pliego-release.jks
password=<la contraseña>
keyAlias=pliego
```

Con ese archivo, `pnpm tauri android build` genera el APK y el AAB ya firmados.
Antes, la primera vez: `./scripts/fetch-pdfium-android.sh`.

## Compilar firmado en GitHub

El workflow `build.yml` lee la clave de dos secretos del repositorio. Se cargan una sola vez,
desde la carpeta del proyecto:

```
base64 -i ~/Documents/"Pliego firma Android"/pliego-release.jks | gh secret set ANDROID_KEYSTORE_BASE64
gh secret set ANDROID_KEYSTORE_PASSWORD < ~/Documents/"Pliego firma Android"/contraseña.txt
```

(También se pueden cargar a mano en GitHub → Settings → Secrets and variables → Actions.)

## Google Play

Al subir la primera versión, Play pide elegir la clave con la que entrega la app ("Firma de apps
de Google Play"). Elige **usar tu propia clave** y sube esta: así el APK de la web y el de Play
llevan la misma firma, y quien instaló uno puede actualizar con el otro. Si en cambio dejas que
Google genere una clave nueva, las dos versiones no se podrán actualizar entre sí.
