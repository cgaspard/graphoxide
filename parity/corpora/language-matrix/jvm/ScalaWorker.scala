package matrix.jvm

class ScalaWorker {
  def process(value: String): String = value.trim
}

class ScalaRunner extends ScalaWorker {
  def execute(value: String): String = process(value)
}
