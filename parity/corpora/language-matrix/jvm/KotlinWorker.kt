package matrix.jvm

open class KotlinWorker {
    open fun process(value: String): String = value.trim()
}

class KotlinRunner : KotlinWorker() {
    override fun process(value: String): String = super.process(value)
}
