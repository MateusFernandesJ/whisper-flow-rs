# whisper-flow-rs

Ditado por voz para Linux usando o Whisper via Groq. Pressione um atalho global para iniciar e encerrar a gravação; o texto transcrito é inserido no aplicativo em foco.

O programa roda como serviço `systemd --user`, funciona em Wayland e X11, não salva o áudio em disco e dá feedback por sons.

## Requisitos

- Linux com uma sessão gráfica Wayland ou X11;
- Rust e Cargo;
- acesso de leitura aos dispositivos em `/dev/input`;
- acesso a `/dev/uinput` para o modo de colagem no Wayland;
- uma chave de API do [Groq](https://console.groq.com/keys);
- `wl-copy` para Wayland e `xdotool` para X11. `ydotool` é opcional.

## Dependências por distribuição

Arch Linux:

```bash
sudo pacman -S --needed base-devel git rust alsa-lib opus pkgconf cmake wl-clipboard xdotool libpulse pipewire-pulse
```

Fedora:

```bash
sudo dnf install cargo gcc gcc-c++ make git alsa-lib-devel opus-devel pkgconf cmake wl-clipboard xdotool pulseaudio-utils pipewire-pulseaudio
```

Linux Mint:

```bash
sudo apt update
sudo apt install build-essential cargo git pkg-config libasound2-dev libopus-dev cmake wl-clipboard xdotool pulseaudio-utils
```

`pactl` e `parec` são usados para capturar o monitor PipeWire/PulseAudio. Em sistemas
com PipeWire, mantenha o serviço de compatibilidade PulseAudio (`pipewire-pulse`) ativo.

Se o programa não conseguir ler o teclado ou criar o teclado virtual, confira as permissões do grupo `input` e faça login novamente após adicioná-lo:

```bash
sudo usermod -aG input "$USER"
echo 'KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' | sudo tee /etc/udev/rules.d/80-uinput.rules
echo uinput | sudo tee /etc/modules-load.d/uinput.conf
sudo modprobe uinput
sudo udevadm control --reload-rules
sudo udevadm trigger
```

Faça logout/login depois de adicionar o usuário ao grupo `input`.

## Compilação e instalação

O arquivo `.make` concentra os comandos de teste, build, instalação, atualização e serviço:

```bash
make -f .make test
make -f .make install
make -f .make permissions
make -f .make restart
make -f .make logs
```

Para baixar uma cópia nova em `~/.local/src/whisper-flow-rs`:

```bash
make -f .make download
cd "$HOME/.local/src/whisper-flow-rs"
make -f .make install
```

Para atualizar uma cópia já clonada, testar, recompilar e reiniciar:

```bash
make -f .make update
```

Os destinos individuais também estão disponíveis: `test`, `build`, `install`, `permissions`,
`restart` e `logs`. Use `make -f .make help` para listá-los.

### Migrar uma instalação antiga

Para migrar uma instalação que ainda usa `whisper-flow`, execute na raiz do projeto:

```bash
DRY_RUN=1 ./scripts/migrate-whisper-flow-rs.sh
./scripts/migrate-whisper-flow-rs.sh
```

O script recompila o release, cria o novo binário/service/configuração, desativa o serviço
antigo e só remove arquivos antigos quando a configuração é idêntica e os binários/service
correspondem aos artefatos esperados. Arquivos alterados são preservados e reportados.

## Configuração

Crie a configuração do usuário a partir do exemplo:

```bash
mkdir -p "$HOME/.config/whisper-flow-rs"
install -Dm600 whisper-flow-rs.env "$HOME/.config/whisper-flow-rs/.env"
```

Edite `~/.config/whisper-flow-rs/.env` e defina a chave do Groq:

```dotenv
GROQ_API_KEY=gsk_...
```

Como alternativa, o programa lê a chave de `~/.config/groq/key`. Variáveis de ambiente do processo têm prioridade sobre o arquivo `.env`. Para usar outro arquivo, defina `WHISPER_ENV` antes de iniciar o programa.

As opções principais são:

| Variável | Padrão | Descrição |
| --- | --- | --- |
| `GROQ_MODEL` | `whisper-large-v3-turbo` | Modelo usado na transcrição. |
| `WHISPER_LANG` | `pt-br` | Idioma da fala; também aceita `auto`, `en`, `es` etc. |
| `WHISPER_HOTKEY` | `ctrl+space` | Atalho global para iniciar/parar (microfone); aceita também combinações só de modificadores, como `ctrl+super`. |
| `WHISPER_HOTKEY_MODE` | `toggle` | Modo do microfone: `toggle` ou `hold`. |
| `WHISPER_HOTKEY2` | `ctrl+shift+space` | Atalho para capturar o áudio de saída; use `off`, `none`, `disabled`, `false` ou `0` para desativar. |
| `WHISPER_HOTKEY2_MODE` | `toggle` | `toggle` inicia/para com dois toques; `hold` captura enquanto o atalho estiver pressionado. |
| `WHISPER_OUTPUT` | `auto` | Saída: `auto`, `paste`, `xdotool` ou `ydotool`. |
| `WHISPER_PASTE` | `ctrl+v` | Combinação usada no modo `paste`: `ctrl+v`, `ctrl+shift+v` ou `shift+insert`. |
| `WHISPER_VOLUME` | `1.0` | Volume dos sons de feedback, de `0.0` a `2.0`. |

O arquivo [`whisper-flow-rs.env`](whisper-flow-rs.env) contém a lista completa de teclas aceitas e exemplos de configuração.

## Executar como serviço

Instale a unidade do systemd e inicie o serviço:

```bash
install -Dm644 whisper-flow-rs.service \
  "$HOME/.config/systemd/user/whisper-flow-rs.service"
systemctl --user daemon-reload
systemctl --user enable --now whisper-flow-rs
```

Veja os logs:

```bash
journalctl --user -u whisper-flow-rs -f
```

Depois de alterar o `.env`:

```bash
systemctl --user restart whisper-flow-rs
```

Para parar e desabilitar o serviço:

```bash
systemctl --user disable --now whisper-flow-rs
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
