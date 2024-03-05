# `cargo-mtime`

This is a small utility that can be used to manage mtimes for
sandboxed compilation with Cargo. Since Cargo relies on file-system
mtime information for invalidating builds, moving files to a temporary
directory for builds will also trigger large rebuilds. There has been
a few attempts at solving this upstream, but nothing has been
stabilized or accepted.

I'm working on supporting Rust in Pants, which uses sha-based caching
at the top already, which means that it is *always* going to be more
conservative for when a build *should happen*. Still, Cargo rebuilds
*more than needed* due to changing paths and updated mtimes, even with
a cache being available. This small utility makes Cargo smarter by
restoring the mtime to the cached mtime of the file.

*How does it work?*

This tool is trivially implemented. It uses a single file to capture
the sha256 of each file at compilation, along with the mtime of
it. When ran again on the same source tree, it'll refresh the database
but also reset the mtime to the timestamp in the database.

To configure it, you can use either positional arguments or environment flags:

``` shellsession
$ CARGO_MTIME_ROOT=. CARGO_MTIME_DB_PATH=~/.cache/mtimes/project.db cargo-mtime-memoize
$ cargo-mtime-memoize . ~/.cache/mtimes/project.db
```

Do not mix databases between projects, it might lead to Cargo not rebuilding correctly.
