XGETTEXT ?= xgettext
MSGINIT ?= msginit
MSGMERGE ?= msgmerge
SED ?= sed
ECHO ?= echo

COPYRIGHT_HOLDER = System Alphabet
VERSION ?= 0.0.1
BUGS_ADDRESS = https://github.com/system-alphabet/systema/issues

SOURCES = $(shell find .. -name "*.rs" -not -path "*/target/*")
REL_SOURCES = $(patsubst ../%,%,$(SOURCES))
POTFILE = messages.pot
PACKAGE_NAME = systema
DEFAULT_CHARSET = UTF-8

# xgettext's Rust lexer matches a function call as `path::to::name (` and a
# macro as `name !`, but never `path::to::name !`.  Everything nested inside a
# path-qualified macro — hence every t_() handed to anyhow::bail! or
# anyhow::anyhow! — is silently skipped (that is how 127 msgids vanished from
# messages.pot).  Rather than reword the 154 affected call sites across 35
# files, extract from a throw-away mirror of the sources in which the path
# prefix of every `a::b!( ... )` is stripped: the mirror is only lexed, never
# compiled, so Rust scoping is irrelevant, and sed rewrites in place so line
# numbers — and therefore the `#:` / `# File:` location comments — stay
# identical.
XGETTEXT_EQUIV = -E 's/\b([A-Za-z_][A-Za-z0-9_]*::)+([A-Za-z_][A-Za-z0-9_]*)!/\2!/g'

$(POTFILE): $(SOURCES)
	@set -eu; \
	mirror=$$(mktemp -d); \
	trap 'rm -rf "$$mirror"' 0 1 2 15; \
	mkdir -p "$$mirror/po"; \
	(cd .. && cp --parents $(REL_SOURCES) "$$mirror"/); \
	$(SED) -i $(XGETTEXT_EQUIV) $$(find "$$mirror" -name '*.rs'); \
	cd "$$mirror/po" && \
	$(XGETTEXT) --language=Rust \
		--keyword=t_:1 \
		--keyword=n_:1,2 \
		--add-comments \
		--from-code=$(DEFAULT_CHARSET) \
		--output=$(abspath $(POTFILE)) \
		--strict \
		--indent \
		--add-location \
		--itstool \
		--no-wrap \
		--copyright-holder="$(COPYRIGHT_HOLDER)" \
		--package-name="$(PACKAGE_NAME)" \
		--package-version="$(VERSION)" \
		--msgid-bugs-address="$(BUGS_ADDRESS)" \
		--verbose \
		$(SOURCES)

.PHONY: new-po
new-po:
	@LANG_VAL="$(LANG)"; \
	if [ -z "$$LANG_VAL" ]; then \
		LANG_VAL="$${LANG}"; \
	fi; \
	if [ -z "$$LANG_VAL" ]; then \
		$(ECHO) "Error: LANG not specified and \$$LANG is empty"; \
		exit 1; \
	fi; \
	case "$$LANG_VAL" in \
		*.*) LANGUAGE="$${LANG_VAL%.*}"; CHARSET="$${LANG_VAL#*.}";; \
		*)   LANGUAGE="$$LANG_VAL"; CHARSET="$(DEFAULT_CHARSET)";; \
	esac; \
	$(ECHO) "Creating $$LANGUAGE.po (charset: $$CHARSET)"; \
	$(MAKE) $(POTFILE); \
	$(MSGINIT) --input=$(POTFILE) --no-wrap --locale=$$LANGUAGE --output=$$LANGUAGE.po --no-translator; \
	$(SED) -i 's/charset=CHARSET/charset='$$CHARSET'/' $$LANGUAGE.po
	$(SED) -i 's/Automatically generated/ChenPi11 <wushengwuxi-msctinoulk@outlook.com>/' $$LANGUAGE.po

# --no-fuzzy-matching: a message that is new to the reference must land as
# plain untranslated, not borrow the msgstr of some merely similar entry.
# Fuzzy matching only ever guesses — for genuinely new msgids it attached 60
# unrelated Chinese strings — and those guesses are what a later translator
# would most likely accept without looking.
.PHONY: update-po
update-po: $(POTFILE)
	@for po in *.po; do \
		if [ -f "$$po" ]; then \
			$(ECHO) "Updating $$po"; \
			$(MSGMERGE) --update --indent --add-location --strict --no-fuzzy-matching --no-wrap --verbose --backup=off "$$po" $(POTFILE); \
		fi; \
	done
	@$(ECHO) "All po files updated."
