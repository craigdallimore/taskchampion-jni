#!/bin/bash

# Install git hooks for pre-commit testing
echo "🔧 Installing git hooks..."

# Configure git to use .githooks directory
git config core.hooksPath .githooks

echo "✅ Git hooks installed!"
echo "📋 The pre-commit hook will now run:"
echo "   - Rust tests (cargo test)"
echo "   - Gradle tests (./gradlew test)"  
echo "   - Rust formatting check (cargo fmt --check)"
echo "   - Rust linting (cargo clippy)"
echo ""
echo "💡 To skip hooks temporarily: git commit --no-verify"