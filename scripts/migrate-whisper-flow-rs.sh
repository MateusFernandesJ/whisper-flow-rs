#!/usr/bin/env bash
set -Eeuo pipefail

# Migra uma instalação anterior whisper-flow para whisper-flow-rs.
# DRY_RUN=1 ./scripts/migrate-whisper-flow-rs.sh apenas mostra as ações.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
PROJECT_DIR=$(cd -- "$SCRIPT_DIR/.." && pwd)
DRY_RUN=${DRY_RUN:-0}

OLD_SERVICE=whisper-flow.service
NEW_SERVICE=whisper-flow-rs.service
OLD_BINARY="$HOME/.local/bin/whisper-flow"
NEW_BINARY="$HOME/.local/bin/whisper-flow-rs"
OLD_CONFIG_DIR="$HOME/.config/whisper-flow"
NEW_CONFIG_DIR="$HOME/.config/whisper-flow-rs"
OLD_CONFIG="$OLD_CONFIG_DIR/.env"
NEW_CONFIG="$NEW_CONFIG_DIR/.env"
OLD_SERVICE_FILE="$HOME/.config/systemd/user/$OLD_SERVICE"
NEW_SERVICE_FILE="$HOME/.config/systemd/user/$NEW_SERVICE"

# Hashes dos artefatos da instalação antiga desta atualização. Se o usuário os
# alterou, o script preserva o arquivo em vez de removê-lo.
OLD_BINARY_SHA256=64f07bba3994ab5fe7fc18abfe4c3a344ce5a71bacb750d9a304fbfd2b5ee884
OLD_SERVICE_SHA256=a118d9dbb9cd3625b41a74b3917888b7d67777fafb9428bd3267d2008e607232

run() {
    if [[ "$DRY_RUN" == 1 ]]; then
        printf '+ '
        printf '%q ' "$@"
        printf '\n'
    else
        "$@"
    fi
}

sha256() {
    sha256sum -- "$1" | awk '{print $1}'
}

remove_if_expected() {
    local path=$1 expected=$2
    if [[ ! -e "$path" ]]; then
        return
    fi
    if [[ ! -f "$path" || "$(sha256 "$path")" != "$expected" ]]; then
        printf 'Preservado (não corresponde ao artefato esperado): %s\n' "$path" >&2
        return
    fi
    run rm -- "$path"
}

printf 'Compilando %s...\n' "$NEW_SERVICE"
run cargo build --release --manifest-path "$PROJECT_DIR/Cargo.toml"

printf 'Instalando a identidade nova...\n'
run install -Dm755 "$PROJECT_DIR/target/release/whisper-flow-rs" "$NEW_BINARY"
run install -Dm644 "$PROJECT_DIR/whisper-flow-rs.service" "$NEW_SERVICE_FILE"

if [[ -e "$NEW_CONFIG" ]]; then
    printf 'Preservando configuração nova: %s\n' "$NEW_CONFIG"
elif [[ -f "$OLD_CONFIG" ]]; then
    run install -Dm600 "$OLD_CONFIG" "$NEW_CONFIG"
    printf 'Configuração antiga copiada para: %s\n' "$NEW_CONFIG"
else
    run install -Dm600 "$PROJECT_DIR/whisper-flow-rs.env" "$NEW_CONFIG"
    printf 'Configuração inicial criada em: %s\n' "$NEW_CONFIG"
fi

if [[ "$DRY_RUN" != 1 ]]; then
    systemctl --user disable --now "$OLD_SERVICE" >/dev/null 2>&1 || true
    systemctl --user daemon-reload
    systemctl --user enable --now "$NEW_SERVICE"
    systemctl --user restart "$NEW_SERVICE"
    systemctl --user is-active --quiet "$NEW_SERVICE"
else
    printf '+ systemctl --user disable --now %q\n' "$OLD_SERVICE"
    printf '+ systemctl --user daemon-reload\n'
    printf '+ systemctl --user enable --now %q\n' "$NEW_SERVICE"
    printf '+ systemctl --user restart %q\n' "$NEW_SERVICE"
fi

# Só remove o .env antigo se a cópia nova for byte a byte igual.
if [[ -f "$OLD_CONFIG" && -f "$NEW_CONFIG" ]]; then
    if cmp -s -- "$OLD_CONFIG" "$NEW_CONFIG"; then
        run rm -- "$OLD_CONFIG"
        run rmdir --ignore-fail-on-non-empty "$OLD_CONFIG_DIR"
    else
        printf 'Preservado (configuração antiga difere da nova): %s\n' "$OLD_CONFIG" >&2
    fi
fi

remove_if_expected "$OLD_SERVICE_FILE" "$OLD_SERVICE_SHA256"
remove_if_expected "$OLD_BINARY" "$OLD_BINARY_SHA256"

printf 'Migração concluída: %s\n' "$NEW_SERVICE"
