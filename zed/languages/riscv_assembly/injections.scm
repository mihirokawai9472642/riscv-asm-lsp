((comment) @injection.content
  (#match? @injection.content "^#\\s*(include|include_next|import|define|undef|if|ifdef|ifndef|elif|elifdef|elifndef|else|endif|error|warning|pragma|line|ident|sccs|assert|unassert|embed)\\b")
  (#set! injection.language "c"))
