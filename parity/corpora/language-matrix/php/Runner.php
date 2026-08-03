<?php
namespace MatrixRuntime;

require_once __DIR__ . '/Contracts.php';

class Runner extends Service
{
    public function execute(string $value): string
    {
        return $this->process($value);
    }
}

class Provider
{
    public function register(): void
    {
        $this->app->bind(Worker::class, Service::class);
    }
}
