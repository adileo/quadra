#!/bin/sh
# The studio (the web interface of the apps, private: github.com/adileo/quadra-studio) built into
# public/app, where the site serves it as a live demo. From STUDIO_SRC, default the checkout next
# to this repository (../studio). Without it the site is built without the demo.
set -e
site="$(cd "$(dirname "$0")/.." && pwd)"
studio="${STUDIO_SRC:-$site/../../studio}"
if [ ! -f "$studio/web/package.json" ]; then
  echo "no studio in $studio (STUDIO_SRC): the site is built without /app" >&2
  exit 0
fi
[ -d "$studio/web/node_modules" ] || npm --prefix "$studio/web" ci --no-audit --no-fund
npm --prefix "$studio/web" run build -- --base /app/ --outDir "$site/public/app" --emptyOutDir
