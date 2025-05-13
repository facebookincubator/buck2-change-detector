# Targets

Run `buck2 targets` with all the special arguments encouraged for use with BTD. Using this helper ensures that as BTD evolves your scripts will continue to work correctly.

As an example:

```shell
supertd targets fbcode//...
```

Within Meta a precompiled version of `supertd` is available as devfeature at
`/usr/local/bin/supertd`.
Devfeature should install automatically on devservers and ondemands, but could also be installed explicitly via `feature install supertd`.

This project relies on the Buck2 features to stream the graph (so it takes constant memory) and error tolerance (so a single error won't break the graph).
