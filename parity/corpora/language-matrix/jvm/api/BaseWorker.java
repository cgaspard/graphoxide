package matrix.api;

public abstract class BaseWorker implements Worker {
    protected String normalize(String value) {
        return value.trim();
    }
}
