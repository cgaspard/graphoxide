# Adversarial language matrix corpus

This deterministic corpus exercises language families that are not represented
deeply in the smaller resolution and identity corpora.  The deliberately
repeated names (`Worker`, `Runner`, `Service`, and `process`) must remain scoped
to their runtime, while imports, calls, inheritance, containment, framework
relationships, and configuration dependencies must still resolve where there
is direct evidence.

The corpus includes Java/Kotlin/Groovy/Scala, Ruby, Dart/Flutter, PHP service
container bindings, Bash sourcing, Terraform/HCL, SQL DDL, C# with project,
XAML, and Razor files, Pascal, Swift/Objective-C/native interop, and container
configuration files. `Dockerfile` and `compose.yaml` are intentional
code-only negative controls: changing whether either file contributes graph
facts changes the reviewed strict graph digest.
