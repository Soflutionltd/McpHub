#!/bin/bash
# Rend un SVG en PNG pour verifier les visuels (librsvg, sans navigateur).
rsvg-convert "$1" -o "${2:-${1%.svg}.png}" && echo "${2:-${1%.svg}.png}"
