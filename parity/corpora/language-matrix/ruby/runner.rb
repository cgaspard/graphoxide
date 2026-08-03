require_relative 'worker'

module MatrixRuntime
  class Runner < Worker
    def execute(value)
      process(value)
    end
  end
end
