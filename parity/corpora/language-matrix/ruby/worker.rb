module MatrixRuntime
  module Audited
    def audit(value)
      value
    end
  end

  class Worker
    include Audited

    def process(value)
      audit(value.strip)
    end
  end
end
