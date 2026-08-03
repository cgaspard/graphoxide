<?php
namespace MatrixRuntime;

interface Worker
{
    public function process(string $value): string;
}

class Service implements Worker
{
    public function process(string $value): string
    {
        return trim($value);
    }
}
