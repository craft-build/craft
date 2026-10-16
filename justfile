# Build the Craft desktop app and package it as a macOS .app bundle.
#
#   just app                     # dist/Craft.app (ad-hoc signed)
#   just dmg                     # dist/Craft-<version>.dmg
#   just identity="Developer ID Application: …" app   # real signature

app_name := "Craft"
bin_name := "craft_code"
bundle_id := "dev.craft.desktop"
version := `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1`
# "-" signs ad-hoc; pass a Developer ID identity to sign for distribution.
identity := "-"

dist := "dist"
app_dir := dist / app_name + ".app"
icon_work := "target/icon"

default: app

# Release build of the GUI binary.
build:
    cargo build --release --bin {{bin_name}}

# AppIcon.icns from Logo.png (padded to a square, then every iconset size).
icon:
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf "{{icon_work}}"
    mkdir -p "{{icon_work}}/AppIcon.iconset"
    w=$(sips -g pixelWidth Logo.png | awk '/pixelWidth/ {print $2}')
    h=$(sips -g pixelHeight Logo.png | awk '/pixelHeight/ {print $2}')
    side=$(( w > h ? w : h ))
    # iconutil needs square images; pad the shorter side with transparency.
    sips --padToHeightWidth "$side" "$side" Logo.png --out "{{icon_work}}/square.png" >/dev/null
    for size in 16 32 128 256 512; do
        sips -z "$size" "$size" "{{icon_work}}/square.png" \
            --out "{{icon_work}}/AppIcon.iconset/icon_${size}x${size}.png" >/dev/null
        double=$(( size * 2 ))
        sips -z "$double" "$double" "{{icon_work}}/square.png" \
            --out "{{icon_work}}/AppIcon.iconset/icon_${size}x${size}@2x.png" >/dev/null
    done
    iconutil -c icns "{{icon_work}}/AppIcon.iconset" -o "{{icon_work}}/AppIcon.icns"

# Assemble and sign dist/Craft.app.
app: build icon
    #!/usr/bin/env bash
    set -euo pipefail
    rm -rf "{{app_dir}}"
    mkdir -p "{{app_dir}}/Contents/MacOS" "{{app_dir}}/Contents/Resources"
    cp "target/release/{{bin_name}}" "{{app_dir}}/Contents/MacOS/{{bin_name}}"
    cp "{{icon_work}}/AppIcon.icns" "{{app_dir}}/Contents/Resources/AppIcon.icns"
    cat > "{{app_dir}}/Contents/Info.plist" <<'PLIST'
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0">
    <dict>
        <key>CFBundleName</key>
        <string>{{app_name}}</string>
        <key>CFBundleDisplayName</key>
        <string>{{app_name}}</string>
        <key>CFBundleIdentifier</key>
        <string>{{bundle_id}}</string>
        <key>CFBundleExecutable</key>
        <string>{{bin_name}}</string>
        <key>CFBundleIconFile</key>
        <string>AppIcon</string>
        <key>CFBundlePackageType</key>
        <string>APPL</string>
        <key>CFBundleShortVersionString</key>
        <string>{{version}}</string>
        <key>CFBundleVersion</key>
        <string>{{version}}</string>
        <key>CFBundleInfoDictionaryVersion</key>
        <string>6.0</string>
        <key>LSMinimumSystemVersion</key>
        <string>11.0</string>
        <key>LSApplicationCategoryType</key>
        <string>public.app-category.developer-tools</string>
        <key>NSHighResolutionCapable</key>
        <true/>
        <key>NSSupportsAutomaticGraphicsSwitching</key>
        <true/>
    </dict>
    </plist>
    PLIST
    # Hardened runtime is required for notarization; harmless for ad-hoc.
    codesign --force --options runtime --timestamp=none --sign "{{identity}}" "{{app_dir}}"
    codesign --verify --strict "{{app_dir}}"
    echo "Built {{app_dir}} ({{version}})"

# Compressed disk image with the app and an /Applications shortcut.
dmg: app
    #!/usr/bin/env bash
    set -euo pipefail
    staging="{{dist}}/dmg"
    rm -rf "$staging" "{{dist}}/{{app_name}}-{{version}}.dmg"
    mkdir -p "$staging"
    cp -R "{{app_dir}}" "$staging/"
    ln -s /Applications "$staging/Applications"
    hdiutil create -volname "{{app_name}}" -srcfolder "$staging" -ov -format UDZO \
        "{{dist}}/{{app_name}}-{{version}}.dmg" >/dev/null
    rm -rf "$staging"
    echo "Built {{dist}}/{{app_name}}-{{version}}.dmg"

# Build the bundle and launch it.
run: app
    open "{{app_dir}}"

clean:
    rm -rf "{{dist}}" "{{icon_work}}"
