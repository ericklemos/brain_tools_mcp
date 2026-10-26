#!/usr/bin/env bash
set -euo pipefail

# Script para download do modelo d3-edge GGUF (0.7B Decision Foundation Model)
# Repositório HuggingFace: ggml-org/vllm-d3-edge-GGUF
# Arquivo: vllm-d3-edge-Q8_0.gguf (~806 MB)

MODEL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/models"
MODEL_NAME="vllm-d3-edge-Q8_0.gguf"
MODEL_PATH="${MODEL_DIR}/${MODEL_NAME}"
MODEL_URL="https://huggingface.co/ggml-org/vllm-d3-edge-GGUF/resolve/main/${MODEL_NAME}"

echo "=========================================================="
echo " Brain-Tools: Download do modelo d3-edge GGUF"
echo "=========================================================="
echo "Destino: ${MODEL_PATH}"
echo "URL:     ${MODEL_URL}"
echo ""

mkdir -p "${MODEL_DIR}"

if [ -f "${MODEL_PATH}" ]; then
    echo "Modelo já existe em: ${MODEL_PATH}"
    FILE_SIZE=$(stat -c%s "${MODEL_PATH}" 2>/dev/null || stat -f%z "${MODEL_PATH}" 2>/dev/null || echo 0)
    if [ "$FILE_SIZE" -gt 800000000 ]; then
        echo "Tamanho verificado (${FILE_SIZE} bytes). Download ignorado."
        exit 0
    else
        echo "Arquivo corrompido ou incompleto (${FILE_SIZE} bytes). Baixando novamente..."
        rm -f "${MODEL_PATH}"
    fi
fi

echo "Iniciando download com curl..."
if command -v curl >/dev/null 2>&1; then
    curl -L --progress-bar -o "${MODEL_PATH}.tmp" "${MODEL_URL}"
    mv "${MODEL_PATH}.tmp" "${MODEL_PATH}"
elif command -v wget >/dev/null 2>&1; then
    wget --show-progress -O "${MODEL_PATH}.tmp" "${MODEL_URL}"
    mv "${MODEL_PATH}.tmp" "${MODEL_PATH}"
else
    echo "Erro: Nem 'curl' nem 'wget' foram encontrados no sistema." >&2
    exit 1
fi

echo ""
echo "Download concluído com sucesso!"
echo "Arquivo: ${MODEL_PATH}"
ls -lh "${MODEL_PATH}"
