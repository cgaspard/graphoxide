def handler(value):
    return value


def register(pool):
    pool.submit(handler)


def alias():
    callback = handler
    return callback


ROUTES = {"event": handler}
