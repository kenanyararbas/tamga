<?php

namespace Acme\PhpLib;

require_once __DIR__ . '/A.php';

class B
{
    public static function shout(): string
    {
        return strtoupper(A::greet());
    }
}
