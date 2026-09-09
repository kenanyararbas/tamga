require_relative "a"

module Shouter
  def self.shout
    Greeter.greet.upcase
  end
end
