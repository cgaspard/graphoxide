package matrix.jvm

class GroovyWorker {
    String process(String value) {
        return value.trim()
    }
}

class GroovyRunner extends GroovyWorker {
    String execute(String value) {
        return process(value)
    }
}
