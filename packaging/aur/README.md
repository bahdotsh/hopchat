# AUR packages for hopchat

Two package bases, following Arch conventions:

| Directory     | AUR package  | Source              | When to use |
|---------------|--------------|---------------------|-------------|
| `hopchat/`    | `hopchat`    | upstream release tag `v$pkgver` | latest release |
| `hopchat-git/`| `hopchat-git`| `main` branch (VCS) | latest commit |

## Who maintains them

Both package bases on the AUR are owned by **9M2PJU**, who contributed this
packaging. Only an AUR maintainer or co-maintainer can push to them. The
files here are the reference copy kept next to the code they build; if the
upstream author needs to push, ask the AUR maintainer to add them as a
co-maintainer first.

## Initial publish (done once per package base)

```sh
# stable
git clone ssh://aur@aur.archlinux.org/hopchat.git /tmp/aur-hopchat
cp packaging/aur/hopchat/PKGBUILD packaging/aur/hopchat/.SRCINFO /tmp/aur-hopchat/
cd /tmp/aur-hopchat && git add PKGBUILD .SRCINFO && git commit -m "init hopchat 0.1.0" && git push

# devel
git clone ssh://aur@aur.archlinux.org/hopchat-git.git /tmp/aur-hopchat-git
cp packaging/aur/hopchat-git/PKGBUILD packaging/aur/hopchat-git/.SRCINFO /tmp/aur-hopchat-git/
cd /tmp/aur-hopchat-git && git add PKGBUILD .SRCINFO && git commit -m "init hopchat-git" && git push
```

## Updating on a new upstream release

```sh
cd packaging/aur/hopchat
# bump pkgver/pkgrel in PKGBUILD, then fill in the real checksum:
updpkgsums
makepkg --printsrcinfo > .SRCINFO
makepkg -si         # verify it builds
# push to AUR as above, and commit the updated files here too
```

The stable PKGBUILD never uses `SKIP`: a fixed release tarball must be
verified. When bumping `pkgver`, `updpkgsums` writes the new sum; until it
runs, the old sum no longer matches and `makepkg` fails closed.

For `hopchat-git`, no update is needed on release; `pkgver()` tracks the
latest commit automatically. Push only when the PKGBUILD itself changes
(dependencies, install paths).

## Notes

- `hopchat` points at `https://github.com/bahdotsh/hopchat/archive/refs/tags/v$pkgver.tar.gz`.
  The reference copy here carries the verified sum for v0.2.0. The copy on
  the AUR must match it: it still had `SKIP` when v0.1.0 was released.
- The AUR packages link the system `openssl`. Only the prebuilt release
  tarballs and the AppImage bundle it (the `vendored-openssl` feature).
- Runtime deps are `gcc-libs` + `openssl` (via `native-tls`); `mdns-sd`
  discovery is pure Rust and needs no Avahi daemon.
