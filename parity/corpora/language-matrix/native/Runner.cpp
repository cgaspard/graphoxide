#include "Worker.h"

const char* run_worker(NativeWorker& worker, const char* value) {
    return worker.process(value);
}
