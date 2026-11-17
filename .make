SHELL := /usr/bin/env bash

REPO_URL ?= https://github.com/MateusFernandesJ/whisper-flow-rs.git
APP_DIR ?= $(HOME)/.local/src/whisper-flow-rs
MAKE_FILE := $(abspath $(lastword $(MAKEFILE_LIST)))
SERVICE := whisper-flow-rs.service
USER_SYSTEMD_DIR := $(HOME)/.config/systemd/user
USER_CONFIG_DIR := $(HOME)/.config/whisper-flow-rs

.PHONY: help download update migrate test build install install-files permissions restart logs

help:
	@printf '%s\n' \
		'make -f .make download      Clona o projeto em APP_DIR' \
		'make -f .make update        Atualiza, testa e executa a migração' \
		'make -f .make migrate       Executa a migração whisper-flow-rs' \
		'make -f .make test          Executa os testes Rust' \
		'make -f .make build         Compila o release' \
		'make -f .make install       Instala binário, unidade e .env inicial' \
		'make -f .make permissions   Configura grupo input e /dev/uinput' \
		'make -f .make restart       Recarrega e reinicia o serviço' \
		'make -f .make logs          Acompanha o Journal do serviço'

download:
	@if [[ -e "$(APP_DIR)" ]]; then \
		echo "Destino já existe: $(APP_DIR)" >&2; \
		exit 1; \
	fi
	mkdir -p "$(dir $(APP_DIR))"
	git clone --depth 1 "$(REPO_URL)" "$(APP_DIR)"
	@echo "Baixado em $(APP_DIR). Entre nesse diretório e execute: make -f .make install"

update:
	git pull --ff-only
	$(MAKE) -f "$(MAKE_FILE)" test
	$(MAKE) -f "$(MAKE_FILE)" migrate

migrate:
	./scripts/migrate-whisper-flow-rs.sh

install:
	$(MAKE) -f "$(MAKE_FILE)" build
	$(MAKE) -f "$(MAKE_FILE)" install-files

install-files:
	install -Dm755 target/release/whisper-flow-rs "$(HOME)/.local/bin/whisper-flow-rs"
	install -Dm644 whisper-flow-rs.service "$(USER_SYSTEMD_DIR)/$(SERVICE)"
	@if [[ ! -e "$(USER_CONFIG_DIR)/.env" ]]; then \
		install -Dm600 whisper-flow-rs.env "$(USER_CONFIG_DIR)/.env"; \
		echo "Configuração criada em $(USER_CONFIG_DIR)/.env"; \
	else \
		echo "Preservando configuração existente em $(USER_CONFIG_DIR)/.env"; \
	fi
	@echo 'Instalação concluída. Execute: make -f .make permissions && make -f .make restart'

test:
	cargo test

build:
	cargo build --release

permissions:
	sudo usermod -aG input "$$USER"
	printf '%s\n' 'KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' | sudo tee /etc/udev/rules.d/80-uinput.rules >/dev/null
	printf '%s\n' uinput | sudo tee /etc/modules-load.d/uinput.conf >/dev/null
	sudo modprobe uinput
	sudo udevadm control --reload-rules
	sudo udevadm trigger
	@echo 'Permissões aplicadas. Faça logout/login para atualizar o grupo input.'

restart:
	systemctl --user daemon-reload
	systemctl --user enable --now "$(SERVICE)"
	systemctl --user restart "$(SERVICE)"

logs:
	journalctl --user -u "$(SERVICE)" -f
