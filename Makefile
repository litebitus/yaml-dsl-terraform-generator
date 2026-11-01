COMPLETION_DIR ?= $(HOME)/.zsh/completions

.PHONY: build completion completion-clean test

build: test
	cargo build --release

test:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

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
