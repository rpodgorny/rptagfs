#!/bin/sh
exec pipenv run coverage run --append rptagfs.py -s "$@"
