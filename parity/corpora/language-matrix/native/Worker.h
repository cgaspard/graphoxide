#pragma once

class NativeWorker {
public:
    virtual const char* process(const char* value) = 0;
};
