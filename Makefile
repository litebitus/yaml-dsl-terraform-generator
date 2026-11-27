COMPLETION_DIR ?= $(HOME)/.zsh/completions
MOTO_IMAGE = ghcr.io/litebitus/motoserver/moto:5.2.6
MOTO_CREDENTIALS = AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1

.PHONY: build completion completion-clean test

build: test
	cargo build --release

test:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	@container="$$(docker run -d --rm -p 127.0.0.1::5000 $(MOTO_IMAGE))" || exit 1; \
	trap 'docker rm -f "$$container" >/dev/null' EXIT; \
	endpoint="http://$$(docker port "$$container" 5000 | head -n 1)"; \
	for attempt in $$(seq 1 120); do \
		curl -sf "$$endpoint/moto-api/" >/dev/null && break; \
		sleep 0.5; \
	done; \
	curl -sf "$$endpoint/moto-api/" >/dev/null || { echo "Moto did not start" >&2; exit 1; }; \
	env -u AWS_PROFILE AWS_ENDPOINT_URL="$$endpoint" $(MOTO_CREDENTIALS) cargo test

completion: build
	@mkdir -p $(COMPLETION_DIR)
	@{ printf '#compdef tfgen ./target/release/tfgen\n'; ./target/release/tfgen completion zsh | tail -n +2; } > $(COMPLETION_DIR)/_tfgen
	@echo "wrote $(COMPLETION_DIR)/_tfgen; binds tfgen and ./target/release/tfgen"
	@grep -q '$(patsubst $(HOME)/%,%,$(COMPLETION_DIR))' $(HOME)/.zshrc 2>/dev/null || \
		echo "add to ~/.zshrc before compinit:  fpath=($(COMPLETION_DIR) \$$fpath)"
	@echo "it shadows a brew-installed tfgen's completion until 'make completion-clean'"
	@printf 'run now: rm -f ~/.zcompdump*; exec zsh\n'

completion-clean:
	rm -f $(COMPLETION_DIR)/_tfgen
	@echo "removed; run 'exec zsh' (rm -f ~/.zcompdump* if the old one lingers)"
