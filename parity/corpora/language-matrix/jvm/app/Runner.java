package matrix.app;

import matrix.api.BaseWorker;
import matrix.api.Worker;

public final class Runner extends BaseWorker {
    private final Worker delegate;

    public Runner(Worker delegate) {
        this.delegate = delegate;
    }

    @Override
    public String process(String value) {
        return delegate.process(normalize(value));
    }
}
