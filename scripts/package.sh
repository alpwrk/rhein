#!/usr/bin/env bash
# Packs a production build into dist/rhein-<version>-linux-x86_64.tar.gz, with a .sha256 beside it:
#
#   scripts/package.sh v0.1.0
#
# The archive holds the binary, the desktop entry, the icon and install.sh, which puts them under
# ~/.local (or /usr with --system). The binary needs the system libraries a Rhein build needs at run
# time (ALSA, Vulkan, xkbcommon, fontconfig, PipeWire and the desktop portal for screen sharing),
# and a glibc at least as new as the one it was built on. The release workflow runs this.
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:?which version? e.g. scripts/package.sh v0.1.0}"
app_id=cc.rheinium.Rhein
[[ -x target/production/rhein ]] || ./install --build-only --no-deps

name="rhein-$version-linux-x86_64"
stage="dist/$name"
rm -rf "$stage" && mkdir -p "$stage"
install -m755 target/production/rhein "$stage/rhein"
install -m644 "packaging/$app_id.desktop" "packaging/$app_id.png" LICENSE "$stage/"
cat > "$stage/install.sh" <<INSTALL
#!/usr/bin/env bash
# Installs Rhein for this user (~/.local), or for everyone with --system (/usr, needs sudo).
set -euo pipefail
here="\$(cd "\$(dirname "\$0")" && pwd)"
prefix="\$HOME/.local" run=()
[[ \${1:-} == --system ]] && prefix=/usr run=(sudo)
"\${run[@]}" install -Dm755 "\$here/rhein" "\$prefix/bin/rhein"
# The full path, as graphical sessions often leave ~/.local/bin off their PATH.
sed "s|^Exec=rhein\\\$|Exec=\$prefix/bin/rhein|" "\$here/$app_id.desktop" \\
  | "\${run[@]}" install -Dm644 /dev/stdin "\$prefix/share/applications/$app_id.desktop"
"\${run[@]}" install -Dm644 "\$here/$app_id.png" "\$prefix/share/icons/hicolor/512x512/apps/$app_id.png"
if command -v update-desktop-database >/dev/null; then
  "\${run[@]}" update-desktop-database -q "\$prefix/share/applications" || true
fi
echo "installed into \$prefix; start it with: rhein"
if [[ \$prefix != /usr && -e /usr/bin/rhein ]]; then
  echo "note: an older install is still in /usr; remove it with:"
  echo "  sudo rm /usr/bin/rhein /usr/share/applications/$app_id.desktop /usr/share/icons/hicolor/512x512/apps/$app_id.png"
fi
case ":\$PATH:" in *":\$prefix/bin:"*) ;; *) echo "note: \$prefix/bin is not on your PATH" ;; esac
INSTALL
chmod +x "$stage/install.sh"
tar -czf "dist/$name.tar.gz" -C dist "$name"
(cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "· packed dist/$name.tar.gz ($(du -h "dist/$name.tar.gz" | cut -f1))"
