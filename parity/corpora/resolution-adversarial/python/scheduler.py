from handlers import on_event


def schedule(pool):
    pool.submit(on_event)
