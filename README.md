# whisper-flow

Ditado por voz para Linux usando o Whisper via Groq. Pressione um atalho global para iniciar e encerrar a gravação; o texto transcrito é inserido no aplicativo em foco.

O programa roda como serviço `systemd --user`, funciona em Wayland e X11, não salva o áudio em disco e dá feedback por sons.

## Requisitos

- Linux com uma sessão gráfica Wayland ou X11;
- Rust e Cargo;
- acesso de leitura aos dispositivos em `/dev/input`;
- acesso a `/dev/uinput` para o modo de colagem no Wayland;
- uma chave de API do [Groq](https://console.groq.com/keys);
- `wl-copy` para Wayland e `xdotool` para X11. `ydotool` é opcional.

Em distribuições baseadas em Debian/Ubuntu:

```bash
sudo apt install build-essential pkg-config libasound2-dev libopus-dev wl-clipboard xdotool
```

Se o programa não conseguir ler o teclado ou criar o teclado virtual, confira as permissões do grupo `input` e faça login novamente após adicioná-lo:

```bash
sudo usermod -aG input "$USER"
```

## Compilação e instalação

```bash
cargo build --release
install -Dm755 target/release/whisper-flow "$HOME/.local/bin/whisper-flow"
```

## Configuração

Crie a configuração do usuário a partir do exemplo:

```bash
mkdir -p "$HOME/.config/whisper-flow"
install -Dm600 whisper-flow.env "$HOME/.config/whisper-flow/.env"
```

Edite `~/.config/whisper-flow/.env` e defina a chave do Groq:

```dotenv
GROQ_API_KEY=gsk_...
```

Como alternativa, o programa lê a chave de `~/.config/groq/key`. Variáveis de ambiente do processo têm prioridade sobre o arquivo `.env`. Para usar outro arquivo, defina `WHISPER_ENV` antes de iniciar o programa.

As opções principais são:

| Variável | Padrão | Descrição |
| --- | --- | --- |
| `GROQ_MODEL` | `whisper-large-v3-turbo` | Modelo usado na transcrição. |
| `WHISPER_LANG` | `pt-br` | Idioma da fala; também aceita `auto`, `en`, `es` etc. |
| `WHISPER_HOTKEY` | `ctrl+space` | Atalho global para iniciar/parar (microfone). |
| `WHISPER_HOTKEY2` | `ctrl+shift+space` | Atalho para capturar o áudio de SAÍDA (headset) e jogar a transcrição só na área de transferência. |
| `WHISPER_OUTPUT` | `auto` | Saída: `auto`, `paste`, `xdotool` ou `ydotool`. |
| `WHISPER_PASTE` | `ctrl+v` | Combinação usada no modo `paste`: `ctrl+v`, `ctrl+shift+v` ou `shift+insert`. |
| `WHISPER_VOLUME` | `1.0` | Volume dos sons de feedback, de `0.0` a `2.0`. |

O arquivo [`whisper-flow.env`](whisper-flow.env) contém a lista completa de teclas aceitas e exemplos de configuração.

## Executar como serviço

Instale a unidade do systemd e inicie o serviço:

```bash
install -Dm644 whisper-flow.service \
  "$HOME/.config/systemd/user/whisper-flow.service"
systemctl --user daemon-reload
systemctl --user enable --now whisper-flow
```

Veja os logs:

```bash
journalctl --user -u whisper-flow -f
```

Depois de alterar o `.env`:

```bash
systemctl --user restart whisper-flow
```

Para parar e desabilitar o serviço:

```bash
systemctl --user disable --now whisper-flow
```

## Uso

1. Coloque o cursor no campo que deve receber o texto.
2. Pressione `Ctrl+Espaço` (ou o atalho configurado).
3. Fale.
4. Pressione o mesmo atalho novamente.

No Wayland, o modo automático copia o texto para o clipboard com `wl-copy` e envia `Ctrl+V` por um teclado virtual, preservando acentos. No X11, usa `xdotool`. Em caso de erro, consulte o log do serviço e verifique se o executável de saída correspondente está instalado.

O programa descarta gravações com menos de 0,3 segundo, encerra automaticamente após 15 minutos e tenta novamente requisições de rede transitórias. O áudio é enviado à API do Groq somente durante a transcrição e não é armazenado localmente.

## Desenvolvimento

Execute os testes unitários com:

```bash
cargo test
```

O projeto é um binário Rust e não possui dependências de runtime além do sistema de áudio, dos dispositivos de entrada e das ferramentas de inserção de texto descritas acima.

## Licença

Nenhuma licença foi definida no projeto até o momento.
