# logsum

`bin/logsum [-q] FILE` counts the lines of a log by level:

```
$ sh bin/logsum examples/app.log
error: 1
warn: 1
info: 3
other: 1
```

With `-q` it prints only the number of errors. It exits 1 when the log has errors, so a
CI step can fail on them. The script is meant to run under any POSIX sh.
