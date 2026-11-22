((comment) @injection.content
  (#match? @injection.content "^#\\s*(include|define|undef|if|ifdef|ifndef|elif|else|endif|error|warning|pragma|line)\\b")
  (#set! injection.language "c"))
